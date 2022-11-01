//! Notify async tasks or threads.
//!
//! This is a synchronization primitive similar to [eventcounts] invented by Dmitry Vyukov.
//!
//! You can use this crate to turn non-blocking data structures into async or blocking data
//! structures. See a [simple mutex] implementation that exposes an async and a blocking interface
//! for acquiring locks.
//!
//! [eventcounts]: http://www.1024cores.net/home/lock-free-algorithms/eventcounts
//! [simple mutex]: https://github.com/smol-rs/event-listener/blob/master/examples/mutex.rs
//!
//! # Examples
//!
//! Wait until another thread sets a boolean flag:
//!
//! ```
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use std::sync::Arc;
//! use std::thread;
//! use std::time::Duration;
//! use std::usize;
//! use event_listener::Event;
//!
//! let flag = Arc::new(AtomicBool::new(false));
//! let event = Arc::new(Event::new());
//!
//! // Spawn a thread that will set the flag after 1 second.
//! thread::spawn({
//!     let flag = flag.clone();
//!     let event = event.clone();
//!     move || {
//!         // Wait for a second.
//!         thread::sleep(Duration::from_secs(1));
//!
//!         // Set the flag.
//!         flag.store(true, Ordering::SeqCst);
//!
//!         // Notify all listeners that the flag has been set.
//!         event.notify(usize::MAX);
//!     }
//! });
//!
//! // Wait until the flag is set.
//! loop {
//!     // Check the flag.
//!     if flag.load(Ordering::SeqCst) {
//!         break;
//!     }
//!
//!     // Start listening for events.
//!     let listener = event.listen();
//!
//!     // Check the flag again after creating the listener.
//!     if flag.load(Ordering::SeqCst) {
//!         break;
//!     }
//!
//!     // Wait for a notification and continue the loop.
//!     listener.wait();
//! }
//! ```

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod listener;
mod queue;
mod sync;
mod util;

use alloc::sync::Arc;

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::task::{Context, Poll};
use core::usize;

use listener::{Listener, Wakeup};
use queue::{ListenerQueue, Node};
use sync::atomic::{self, AtomicUsize, Ordering};
use util::RacyArc;

#[cfg(feature = "std")]
use std::time::{Duration, Instant};

#[cfg(feature = "std")]
use std::panic::{RefUnwindSafe, UnwindSafe};

/// Inner state of [`Event`].
struct Inner {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    notified: AtomicUsize,

    /// The number of entries that haven't been orphaned yet.
    len: AtomicUsize,

    /// The queue that holds all listeners.
    queue: ListenerQueue,
}

/// A synchronization primitive for notifying async tasks and threads.
///
/// Listeners can be registered using [`Event::listen()`]. There are two ways to notify listeners:
///
/// 1. [`Event::notify()`] notifies a number of listeners.
/// 2. [`Event::notify_additional()`] notifies a number of previously unnotified listeners.
///
/// If there are no active listeners at the time a notification is sent, it simply gets lost.
///
/// There are two ways for a listener to wait for a notification:
///
/// 1. In an asynchronous manner using `.await`.
/// 2. In a blocking manner by calling [`EventListener::wait()`] on it.
///
/// If a notified listener is dropped without receiving a notification, dropping will notify
/// another active listener. Whether one *additional* listener will be notified depends on what
/// kind of notification was delivered.
///
/// Listeners are registered and notified in the first-in first-out fashion, ensuring fairness.
pub struct Event {
    inner: util::RacyArc<Inner>,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

#[cfg(feature = "std")]
impl UnwindSafe for Event {}
#[cfg(feature = "std")]
impl RefUnwindSafe for Event {}

impl Event {
    /// Creates a new [`Event`].
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// ```
    #[inline]
    pub const fn new() -> Event {
        Event {
            inner: RacyArc::new(),
        }
    }

    /// Returns a guard listening for a notification.
    ///
    /// This method emits a `SeqCst` fence after registering a listener.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    /// ```
    #[cold]
    pub fn listen(&self) -> EventListener {
        // Initialize the inner state if we haven't already.
        let inner = self.inner.get_or_init(|| {
            Arc::new(Inner {
                notified: AtomicUsize::new(0),
                len: AtomicUsize::new(0),
                queue: ListenerQueue::new(),
            })
        });

        let listener = EventListener {
            inner: Arc::clone(&inner),
            entry: Some(inner.queue.push(Listener::new())),
        };

        // Bump the number of listeners.
        inner.len.fetch_add(1, Ordering::SeqCst);

        // Make sure the listener is registered before whatever happens next.
        full_fence();
        listener
    }

    /// Notifies a number of active listeners.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify_additional()`], this method only makes sure *at least* `n`
    /// listeners among the active ones are notified.
    ///
    /// This method emits a `SeqCst` fence before notifying listeners.
    ///
    /// # Behavior
    ///
    /// If this method is called at the same time as [`notify()`] or [`notify_additional()`]
    /// is called on another thread, it may wake more listeners than `n`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(2);
    /// ```
    #[inline]
    pub fn notify(&self, n: usize) {
        // Make sure the notification comes after whatever triggered it.
        full_fence();

        if let Some(inner) = self.inner.get() {
            // Notify if there is at least one unnotified listener and the number of notified
            // listeners is less than `n`.
            let notified = inner.notified.load(Ordering::Acquire);
            if notified < n {
                inner.notify(n - notified, false);
            }
        }
    }

    /// Notifies a number of active listeners without emitting a `SeqCst` fence.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify_additional()`], this method only makes sure *at least* `n`
    /// listeners among the active ones are notified.
    ///
    /// Unlike [`Event::notify()`], this method does not emit a `SeqCst` fence.
    ///
    /// # Behavior
    ///
    /// If this method is called at the same time as [`notify()`] or [`notify_additional()`]
    /// is called on another thread, it may wake more listeners than `n`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(2);
    /// ```
    #[inline]
    pub fn notify_relaxed(&self, n: usize) {
        if let Some(inner) = self.inner.get() {
            // Notify if there is at least one unnotified listener and the number of notified
            // listeners is less than `n`.
            let notified = inner.notified.load(Ordering::Acquire);
            if notified < n {
                inner.notify(n - notified, false);
            }
        }
    }

    /// Notifies a number of active and still unnotified listeners.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify()`], this method will notify `n` *additional* listeners that
    /// were previously unnotified.
    ///
    /// This method emits a `SeqCst` fence before notifying listeners.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify_additional(1);
    /// event.notify_additional(1);
    /// ```
    #[inline]
    pub fn notify_additional(&self, n: usize) {
        // Make sure the notification comes after whatever triggered it.
        full_fence();

        if let Some(inner) = self.inner.get() {
            // Notify if there is at least one unnotified listener.
            if inner.len.load(Ordering::Acquire) > 0 {
                inner.notify(n, true);
            }
        }
    }

    /// Notifies a number of active and still unnotified listeners without emitting a `SeqCst`
    /// fence.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify()`], this method will notify `n` *additional* listeners that
    /// were previously unnotified.
    ///
    /// Unlike [`Event::notify_additional()`], this method does not emit a `SeqCst` fence.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify_additional_relaxed(1);
    /// event.notify_additional_relaxed(1);
    /// ```
    #[inline]
    pub fn notify_additional_relaxed(&self, n: usize) {
        if let Some(inner) = self.inner.get() {
            // Notify if there is at least one unnotified listener.
            if inner.len.load(Ordering::Acquire) > 0 {
                inner.notify(n, true);
            }
        }
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Event { .. }")
    }
}

impl Default for Event {
    fn default() -> Event {
        Event::new()
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // Notify all remaining listeners.
        if let Some(inner) = self.inner.get() {
            while let Some(entry) = inner.queue.pop() {
                unsafe {
                    entry.as_ref().listener().notify(false);
                }
            }
        }
    }
}

/// A guard waiting for a notification from an [`Event`].
///
/// There are two ways for a listener to wait for a notification:
///
/// 1. In an asynchronous manner using `.await`.
/// 2. In a blocking manner by calling [`EventListener::wait()`] on it.
///
/// If a notified listener is dropped without receiving a notification, dropping will notify
/// another active listener. Whether one *additional* listener will be notified depends on what
/// kind of notification was delivered.
pub struct EventListener {
    /// A reference to [`Event`]'s inner state.
    inner: Arc<Inner>,

    /// A pointer to this listener's entry in the linked list.
    entry: Option<NonNull<Node>>,
}

unsafe impl Send for EventListener {}
unsafe impl Sync for EventListener {}

#[cfg(feature = "std")]
impl UnwindSafe for EventListener {}
#[cfg(feature = "std")]
impl RefUnwindSafe for EventListener {}

#[cfg(feature = "std")]
impl EventListener {
    /// Blocks until a notification is received.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// // Notify `listener`.
    /// event.notify(1);
    ///
    /// // Receive the notification.
    /// listener.wait();
    /// ```
    pub fn wait(self) {
        self.wait_internal(None);
    }

    /// Blocks until a notification is received or a timeout is reached.
    ///
    /// Returns `true` if a notification was received.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(!listener.wait_timeout(Duration::from_secs(1)));
    /// ```
    pub fn wait_timeout(self, timeout: Duration) -> bool {
        self.wait_internal(Some(Instant::now() + timeout))
    }

    /// Blocks until a notification is received or a deadline is reached.
    ///
    /// Returns `true` if a notification was received.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(!listener.wait_deadline(Instant::now() + Duration::from_secs(1)));
    /// ```
    pub fn wait_deadline(self, deadline: Instant) -> bool {
        self.wait_internal(Some(deadline))
    }

    fn wait_internal(mut self, deadline: Option<Instant>) -> bool {
        // Take out the entry pointer and set it to `None`.
        let entry = match self.entry.take() {
            None => unreachable!("cannot wait twice on an `EventListener`"),
            Some(entry) => entry,
        };
        let (parker, unparker) = parking::pair();

        // Wait until a notification is received or the timeout is reached.
        loop {
            // See if we've been notified.
            if unsafe {
                entry
                    .as_ref()
                    .listener()
                    .register(|| Wakeup::Thread(unparker.clone()))
            } {
                // We've been notified, so we're done.
                self.decrement_length();
                self.decrement_notified();
                return true;
            }

            match deadline {
                None => parker.park(),

                Some(deadline) => {
                    // Check for timeout.
                    let now = Instant::now();

                    // Park until the deadline.
                    let notified = parker.park_timeout(
                        deadline
                            .checked_duration_since(now)
                            .unwrap_or_else(|| Duration::from_secs(0)),
                    );

                    if !notified {
                        // We timed out, so we're done.
                        self.decrement_length();
                        return false;
                    }
                }
            }
        }
    }
}

impl EventListener {
    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    ///
    /// Returns `true` if a notification was discarded.
    ///
    /// # Examples
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// event.notify(1);
    ///
    /// assert!(listener1.discard());
    /// assert!(!listener2.discard());
    /// ```
    pub fn discard(mut self) -> bool {
        self.orphan().is_some()
    }

    /// Returns `true` if this listener listens to the given `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// assert!(listener.listens_to(&event));
    /// ```
    #[inline]
    pub fn listens_to(&self, event: &Event) -> bool {
        ptr::eq::<Inner>(&*self.inner, event.inner.as_ptr())
    }

    /// Returns `true` if both listeners listen to the same `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// assert!(listener1.same_event(&listener2));
    /// ```
    pub fn same_event(&self, other: &EventListener) -> bool {
        ptr::eq::<Inner>(&*self.inner, &*other.inner)
    }

    /// Orphans this listener and updates counts in the main struct.
    ///
    /// Returns Some() if the listener was notified, None otherwise.
    fn orphan(&mut self) -> Option<bool> {
        if let Some(node) = self.entry.take() {
            self.decrement_length();
            let orphan = unsafe { self.inner.queue.orphan(node) };

            // If the listener was notified, update the counts.
            if let Some(additional) = orphan {
                self.decrement_notified();
                return Some(additional);
            }
        }

        None
    }

    /// Decrement the length of the queue.
    fn decrement_length(&self) {
        self.inner.len.fetch_sub(1, Ordering::Release);
    }

    /// Decrement the number of notified listeners.
    fn decrement_notified(&self) {
        self.inner.notified.fetch_sub(1, Ordering::Release);
    }
}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Future for EventListener {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let entry = match self.entry {
            None => unreachable!("cannot poll a completed `EventListener` future"),
            Some(entry) => entry,
        };

        // See if we've been notified.
        if unsafe {
            entry
                .as_ref()
                .listener()
                .register(|| Wakeup::Waker(cx.waker().clone()))
        } {
            // We've been notified, so we're done.
            // Remove ourselves from the list.
            self.get_mut().orphan();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        // Orphan the listener.
        if let Some(additional) = self.orphan() {
            // If a notification was delivered, then pass it on to the next listener.
            if additional || self.inner.notified.load(Ordering::Acquire) == 0 {
                self.inner.notify(1, additional);
            }
        }
    }
}

impl Inner {
    /// Notify `n` listeners that have not yet been notified.
    #[cold]
    fn notify(&self, mut n: usize, additional: bool) {
        while n > 0 {
            // Try to notify a listener.
            let listener = match self.queue.pop() {
                Some(listener) => listener,
                None => break,
            };

            n -= 1;

            // Notify the listener.
            let notified = unsafe { listener.as_ref().listener().notify(additional) };

            if notified {
                self.notified.fetch_add(1, Ordering::Release);
            }
        }
    }
}

/// Equivalent to `atomic::fence(Ordering::SeqCst)`, but in some cases faster.
#[inline]
fn full_fence() {
    if cfg!(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        not(miri)
    )) {
        // HACK(stjepang): On x86 architectures there are two different ways of executing
        // a `SeqCst` fence.
        //
        // 1. `atomic::fence(SeqCst)`, which compiles into a `mfence` instruction.
        // 2. `_.compare_exchange(_, _, SeqCst, SeqCst)`, which compiles into a `lock cmpxchg` instruction.
        //
        // Both instructions have the effect of a full barrier, but empirical benchmarks have shown
        // that the second one is sometimes a bit faster.
        //
        // The ideal solution here would be to use inline assembly, but we're instead creating a
        // temporary atomic variable and compare-and-exchanging its value. No sane compiler to
        // x86 platforms is going to optimize this away.
        atomic::compiler_fence(Ordering::SeqCst);
        let a = AtomicUsize::new(0);
        let _ = a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        atomic::compiler_fence(Ordering::SeqCst);
    } else {
        atomic::fence(Ordering::SeqCst);
    }
}

#[inline]
fn yield_now() {
    #[cfg(not(feature = "std"))]
    {
        #[allow(deprecated)]
        sync::atomic::spin_loop_hint();
    }

    #[cfg(feature = "std")]
    std::thread::yield_now();
}
