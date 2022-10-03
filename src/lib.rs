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
//! # // Sleep to prevent MIRI failure, https://github.com/rust-lang/miri/issues/1371
//! # std::thread::sleep(std::time::Duration::from_secs(3));
//! ```
//!
//! # Features
//!
//! There is a `portable-atomic` feature, which uses a polyfill from the
//! [`portable-atomic`] crate to provide atomic operations on platforms that do not support them.
//! See the [`README`] for the [`portable-atomic`] crate for more information on how to use it on
//! single-threaded targets. Note that even with this feature enabled, `event-listener` still
//! requires a global allocator to be available. See the documentation for the
//! [`std::alloc::GlobalAlloc`] trait for more information.
//!
//! [`portable-atomic`]: https://crates.io/crates/portable-atomic
//! [`README`]: https://github.com/taiki-e/portable-atomic/blob/main/README.md#optional-cfg

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod listener;
mod sync;

use alloc::sync::Arc;

use core::fmt;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::ptr::{self, NonNull};
use core::task::{Context, Poll};

#[cfg(all(feature = "std", not(loom)))]
use std::time::Duration;
#[cfg(feature = "std")]
use std::time::Instant;

use concurrent_queue::ConcurrentQueue;

use listener::{CachedListener, Listener, Task};
use sync::atomic::Ordering::{self, AcqRel, Acquire, Release};
use sync::atomic::{AtomicPtr, AtomicUsize};

#[cfg(not(loom))]
use sync::AtomicWithMut;

#[cfg(feature = "std")]
use sync::pair;

/// Inner state for an `Event`.
struct Inner {
    /// The queue that contains the listeners.
    ///
    /// The pointers to the listeners are either heap-allocated or stored in the cache.
    queue: ConcurrentQueue<NonNull<Listener>>,

    /// A cached listener, used to avoid unnecessary heap allocation.
    cached: CachedListener,

    /// The number of non-notified entries in the queue.
    len: AtomicUsize,

    /// The number of notified entries in the queue.
    notified: AtomicUsize,
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
    /// The linked list containing the event listeners.
    ///
    /// Semantically, this is an `Option<Arc<Inner>>` that can be atomically
    /// const-initialized.
    inner: AtomicPtr<Inner>,
}

impl Event {
    /// Creates a new [`Event`].
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Creates a new [`Event`].
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Returns a guard listening for a notification.
    pub fn listen(&self) -> EventListener {
        // Load or get a reference to the listener.
        let inner = unsafe { ManuallyDrop::new(Arc::from_raw(self.inner())) };

        // Create a new listener in the queue.
        let entry = Listener::alloc(&inner);
        inner.queue.push(entry).ok();

        let listener = EventListener {
            inner: Arc::clone(&*inner),
            entry: Some(entry),
        };

        // Update the non-notified length.
        inner.len.fetch_add(1, Release);

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

        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener and the number of notified
            // listeners is less than `n`.
            inner.notify(n);
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
        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener and the number of notified
            // listeners is less than `n`.
            inner.notify(n);
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

        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener.
            if inner.len.load(Acquire) > 0 {
                inner.notify_additional(n);
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
        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener.
            if inner.len.load(Acquire) > 0 {
                inner.notify_additional(n);
            }
        }
    }

    /// Returns a reference to the inner state if it was initialized.
    #[inline]
    fn try_inner(&self) -> Option<&Inner> {
        let inner = self.inner.load(Ordering::Acquire);
        unsafe { inner.as_ref() }
    }

    /// Get the pointer to the linked list of listeners.
    fn inner(&self) -> *const Inner {
        // If the pointer is null, try to initialize it.
        let mut inner = self.inner.load(Acquire);

        if inner.is_null() {
            // Allocate on the heap.
            let new = Arc::new(Inner {
                queue: ConcurrentQueue::unbounded(),
                cached: CachedListener::default(),
                len: AtomicUsize::new(0),
                notified: AtomicUsize::new(0),
            });

            // Convert to a pointer.
            let new = Arc::into_raw(new) as *mut Inner;

            // Attempt to replace the original state with the new pointer.
            inner = self
                .inner
                .compare_exchange(inner, new, AcqRel, Acquire)
                .unwrap_or_else(|e| e);

            // Check if the old pointer value was indeed null.
            if inner.is_null() {
                // If yes, then use the new state pointer.
                inner = new;
            } else {
                // If not, that means a concurrent operation has initialized the state.
                // In that case, use the old pointer and deallocate the new one.
                unsafe {
                    drop(Arc::from_raw(new));
                }
            }
        }

        inner
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Event { .. }")
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // Drop the inner state.
        self.inner.with_mut(|inner| {
            if !inner.is_null() {
                // Get the Arc and notify all remaining listeners.
                let inner = unsafe { Arc::<Inner>::from_raw(*inner) };

                while let Ok(listener) = inner.queue.pop() {
                    Listener::notify(listener, false, &inner);
                }

                drop(inner);
            }
        });
    }
}

/// A guard waiting for a notification from an [`Event`].
pub struct EventListener {
    /// The reference to the original linked list.
    inner: Arc<Inner>,

    /// The specific entry that this listener is listening on.
    entry: Option<NonNull<Listener>>,
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
        ptr::eq::<Inner>(&*self.inner, event.inner.load(Ordering::Acquire))
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

    /// A wrapper around `entry.orphan()` that also updates counts in the main structure.
    fn orphan(&mut self) -> Option<bool> {
        self.inner.len.fetch_sub(1, Release);

        if let Some(entry) = self.entry.take() {
            if let Some(additional) = Listener::orphan(entry, &self.inner) {
                // Decrement the number of notified entries.
                self.inner.notified.fetch_sub(1, Release);
                return Some(additional);
            }
        }

        None
    }
}

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
    #[cfg(not(loom))]
    pub fn wait_timeout(self, duration: Duration) -> bool {
        self.wait_internal(Instant::now().checked_add(duration))
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
    #[cfg(not(loom))]
    pub fn wait_deadline(self, deadline: Instant) -> bool {
        self.wait_internal(Some(deadline))
    }

    fn wait_internal(mut self, deadline: Option<Instant>) -> bool {
        // Take out the entry.
        let entry = self.entry.expect("already waited");

        // Create a parker/unparker pair.
        let (parker, unparker) = pair();

        // Begin looping on `entry.wait()`.
        let notified = loop {
            let unparker = unparker.clone();

            if Listener::wait(entry, move || Task::Thread(unparker)) {
                // We have been notified.
                break true;
            }

            // Park the thread and see if we have been notified.
            match deadline {
                Some(deadline) => {
                    #[cfg(loom)]
                    {
                        let _ = deadline;
                        panic!("`wait_deadline` is not supported with loom");
                    }

                    #[cfg(not(loom))]
                    {
                        if !parker.park_deadline(deadline) {
                            // The timeout elapsed. Return false.
                            break false;
                        }
                    }
                }
                None => parker.park(),
            }
        };

        self.orphan();
        notified
    }
}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Unpin for EventListener {}

impl Future for EventListener {
    type Output = ();

    fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let entry = self.entry.expect("already waited");

        if Listener::wait(entry, || Task::Waker(cx.waker().clone())) {
            self.orphan();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        // If this listener has never picked up a notification, but a notification was delivered to it...
        if let Some(additional) = self.orphan() {
            // Then pass it on to another active listener.
            if additional {
                self.inner.notify_additional(1);
            } else {
                self.inner.notify(1);
            }
        }
    }
}

impl Inner {
    /// Notifies `n` listeners.
    fn notify(&self, n: usize) {
        let notified = self.notified.load(Acquire);

        if notified >= n {
            return;
        }

        self.notify_internal(n - notified, false)
    }

    /// Notifies `n` additional listeners.
    fn notify_additional(&self, n: usize) {
        self.notify_internal(n, true)
    }

    #[cold]
    fn notify_internal(&self, mut n: usize, additional: bool) {
        while n > 0 {
            n -= 1;

            // Notify the first unnotified entry.
            match self.queue.pop() {
                Err(_) => break,
                Ok(entry) => {
                    if Listener::notify(entry, additional, self) {
                        // Increment the number of notified entries.
                        self.notified.fetch_add(1, Release);
                    }
                }
            }
        }
    }
}

/// Indicate to the compiler/scheduler that we're in a spin loop.
fn busy_wait() {
    #[cfg(feature = "std")]
    sync::thread::yield_now();

    #[allow(deprecated)]
    #[cfg(not(feature = "std"))]
    core::sync::atomic::spin_loop_hint();
}

#[inline]
fn full_fence() {
    if cfg!(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        not(miri),
        not(loom)
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
        sync::atomic::compiler_fence(Ordering::SeqCst);
        let a = AtomicUsize::new(0);
        let _ = a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        sync::atomic::compiler_fence(Ordering::SeqCst);
    } else {
        sync::atomic::fence(Ordering::SeqCst);
    }
}
