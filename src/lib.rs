//! Notify async tasks or threads.
//!
//! This is a synchronization primitive similar to [eventcounts] invented by Dmitry Vyukov.
//!
//! You can use this crate to turn non-blocking data structures into async or blocking data
//! structures. See a [simple mutex] implementation that exposes an async and a blocking interface
//! for acquiring locks.
//!
//! [eventcounts]: https://www.1024cores.net/home/lock-free-algorithms/eventcounts
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

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod list;
mod sync;

use alloc::sync::Arc;

use core::fmt;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll, Waker};
use core::usize;

use sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering};

#[cfg(feature = "std")]
use std::panic::{RefUnwindSafe, UnwindSafe};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

#[cfg(feature = "std")]
use parking::Unparker;

/// An asynchronous waker or thread unparker that can be used to notify a task or thread.
#[derive(Debug)]
enum Task {
    /// A waker that can be used to notify a task.
    Waker(Waker),

    /// An unparker that can be used to notify a thread.
    #[cfg(feature = "std")]
    Thread(Unparker),
}

impl Task {
    fn as_task_ref(&self) -> TaskRef<'_> {
        match self {
            Self::Waker(waker) => TaskRef::Waker(waker),
            #[cfg(feature = "std")]
            Self::Thread(unparker) => TaskRef::Unparker(unparker),
        }
    }

    /// Notifies the task or thread.
    fn wake(self) {
        match self {
            Task::Waker(waker) => waker.wake(),
            #[cfg(feature = "std")]
            Task::Thread(unparker) => {
                unparker.unpark();
            }
        }
    }
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.as_task_ref().will_wake(other.as_task_ref())
    }
}

/// A reference to a task.
#[derive(Debug, Clone, Copy)]
enum TaskRef<'a> {
    /// A waker that wakes up a future.
    Waker(&'a Waker),

    /// An unparker that wakes up a thread.
    #[cfg(feature = "std")]
    Unparker(&'a parking::Unparker),
}

impl TaskRef<'_> {
    /// Tells if this task will wake up the other task.
    fn will_wake(self, other: Self) -> bool {
        match (self, other) {
            (Self::Waker(a), Self::Waker(b)) => a.will_wake(b),
            #[cfg(feature = "std")]
            (Self::Unparker(_), Self::Unparker(_)) => {
                // TODO: Use unreleased will_unpark API.
                false
            }
            #[cfg(feature = "std")]
            _ => false,
        }
    }

    /// Converts this task reference to a task by cloning.
    fn into_task(self) -> Task {
        match self {
            Self::Waker(waker) => Task::Waker(waker.clone()),
            #[cfg(feature = "std")]
            Self::Unparker(unparker) => Task::Thread(unparker.clone()),
        }
    }
}

/// Inner state of [`Event`].
struct Inner {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    notified: AtomicUsize,

    /// Inner queue of event listeners.
    list: list::List,
}

impl Inner {
    /// Create a new `Inner`.
    fn new() -> Self {
        Self {
            notified: AtomicUsize::new(core::usize::MAX),
            list: list::List::new(),
        }
    }
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
    /// A pointer to heap-allocated inner state.
    ///
    /// This pointer is initially null and gets lazily initialized on first use. Semantically, it
    /// is an `Arc<Inner>` so it's important to keep in mind that it contributes to the [`Arc`]'s
    /// reference count.
    inner: AtomicPtr<Inner>,
}

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
            inner: AtomicPtr::new(ptr::null_mut()),
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
        let inner = self.inner();

        // Register the listener.
        let mut listener = EventListener {
            inner: unsafe { Arc::clone(&ManuallyDrop::new(Arc::from_raw(inner))) },
            state: list::Listener::Discarded,
        };

        listener.inner.insert(&mut listener.state);

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
            if inner.notified.load(Ordering::Acquire) < n {
                inner.notify(n, false);
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
            if inner.notified.load(Ordering::Acquire) < n {
                inner.notify(n, false);
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

        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener.
            if inner.notified.load(Ordering::Acquire) < usize::MAX {
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
        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener.
            if inner.notified.load(Ordering::Acquire) < usize::MAX {
                inner.notify(n, true);
            }
        }
    }

    /// Returns a reference to the inner state if it was initialized.
    #[inline]
    fn try_inner(&self) -> Option<&Inner> {
        let inner = self.inner.load(Ordering::Acquire);
        unsafe { inner.as_ref() }
    }

    /// Returns a raw pointer to the inner state, initializing it if necessary.
    ///
    /// This returns a raw pointer instead of reference because `from_raw`
    /// requires raw/mut provenance: <https://github.com/rust-lang/rust/pull/67339>
    fn inner(&self) -> *const Inner {
        let mut inner = self.inner.load(Ordering::Acquire);

        // Initialize the state if this is its first use.
        if inner.is_null() {
            // Allocate on the heap.
            let new = Arc::new(Inner::new());
            // Convert the heap-allocated state into a raw pointer.
            let new = Arc::into_raw(new) as *mut Inner;

            // Attempt to replace the null-pointer with the new state pointer.
            inner = self
                .inner
                .compare_exchange(inner, new, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|x| x);

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

impl Drop for Event {
    #[inline]
    fn drop(&mut self) {
        let inner: *mut Inner = *self.inner.get_mut();

        // If the state pointer has been initialized, deallocate it.
        if !inner.is_null() {
            unsafe {
                drop(Arc::from_raw(inner));
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

    /// The current state of the listener.
    state: list::Listener,
}

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
        let (parker, unparker) = parking::pair();

        // Set the listener's state to `Task`.
        match self
            .inner
            .register(&mut self.state, TaskRef::Unparker(&unparker))
        {
            Some(true) => {
                // We were already notified, so we don't need to park.
                return true;
            }

            Some(false) => {
                // We're now waiting for a notification.
            }

            None => {
                // We were never inserted into the list.
                panic!("listener was never inserted into the list");
            }
        }

        // Wait until a notification is received or the timeout is reached.
        loop {
            match deadline {
                None => parker.park(),

                Some(deadline) => {
                    // Check for timeout.
                    let now = Instant::now();
                    if now >= deadline {
                        // Remove our entry and check if we were notified.
                        return self
                            .inner
                            .remove(&mut self.state, false)
                            .expect("We never removed ourself from the list")
                            .is_notified();
                    }

                    // Park until the deadline.
                    parker.park_timeout(deadline - now);
                }
            }

            // See if we were notified.
            if self
                .inner
                .register(&mut self.state, TaskRef::Unparker(&unparker))
                .expect("We never removed ourself from the list")
            {
                return true;
            }
        }
    }
}

impl EventListener {
    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    ///
    /// Returns `true` if a notification was discarded. Note that this function may spuriously
    /// return `false` even if a notification was received by the listener.
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
        self.inner
            .remove(&mut self.state, false)
            .map_or(false, |state| state.is_notified())
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
}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Future for EventListener {
    type Output = ();

    #[allow(unreachable_patterns)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Try to register the listener.
        match this
            .inner
            .register(&mut this.state, TaskRef::Waker(cx.waker()))
        {
            Some(true) => {
                // We were already notified, so we don't need to park.
                Poll::Ready(())
            }

            Some(false) => {
                // We're now waiting for a notification.
                Poll::Pending
            }

            None => {
                // We were never inserted into the list.
                panic!("listener was never inserted into the list");
            }
        }
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        self.inner.remove(&mut self.state, true);
    }
}

/// The state of a listener.
#[derive(Debug, PartialEq)]
pub(crate) enum State {
    /// It has just been created.
    Created,

    /// It has received a notification.
    ///
    /// The `bool` is `true` if this was an "additional" notification.
    Notified(bool),

    /// A task is polling it.
    Task(Task),

    /// Empty hole used to replace a notified listener.
    NotifiedTaken,
}

impl State {
    /// Returns `true` if this is the `Notified` state.
    #[inline]
    pub(crate) fn is_notified(&self) -> bool {
        match self {
            State::Notified(_) | Self::NotifiedTaken => true,
            _ => false,
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
