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
//!     let mut listener = event.listen();
//!
//!     // Check the flag again after creating the listener.
//!     if flag.load(Ordering::SeqCst) {
//!         break;
//!     }
//!
//!     // Wait for a notification and continue the loop.
//!     listener.as_mut().wait();
//! }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod inner;
mod list;
mod sync;

use core::fmt;
use core::future::Future;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{self, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use core::usize;

use sync::cell::UnsafeCell;

#[cfg(feature = "std")]
use std::panic::{RefUnwindSafe, UnwindSafe};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

use inner::Inner;
use list::{Entry, State};

#[cfg(feature = "std")]
use parking::Unparker;

/// An asynchronous waker or thread unparker that can be used to notify a task or thread.
enum Task {
    /// A waker that can be used to notify a task.
    Waker(Waker),

    /// An unparker that can be used to notify a thread.
    #[cfg(feature = "std")]
    Thread(Unparker),
}

impl Task {
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
    /// The inner state of the event.
    inner: Inner,
}

#[cfg(feature = "std")]
impl UnwindSafe for Event {}
#[cfg(feature = "std")]
impl RefUnwindSafe for Event {}

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

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
            inner: Inner::new(),
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
    #[cfg(feature = "alloc")]
    #[cold]
    pub fn listen(&self) -> Pin<alloc::boxed::Box<EventListener<'_>>> {
        // Pin the event listener to the heap and initialize it.
        let mut listener = alloc::boxed::Box::pin(EventListener::new());
        listener.as_mut().listen_to(self);
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

        // Notify if there is at least one unnotified listener and the number of notified
        // listeners is less than `n`.
        if self.inner.notified.load(Ordering::Acquire) < n {
            self.inner.lock().notify_unnotified(n);
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
        // Notify if there is at least one unnotified listener and the number of notified
        // listeners is less than `n`.
        if self.inner.notified.load(Ordering::Acquire) < n {
            self.inner.lock().notify_unnotified(n);
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

        // Notify if there is at least one unnotified listener.
        if self.inner.notified.load(Ordering::Acquire) < usize::MAX {
            self.inner.lock().notify_additional(n);
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
        // Notify if there is at least one unnotified listener.
        if self.inner.notified.load(Ordering::Acquire) < usize::MAX {
            self.inner.lock().notify_additional(n);
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
pub struct EventListener<'a> {
    /// A reference to [`Event`]'s inner state.
    inner: Option<&'a Inner>,

    /// Our entry in the intrusive linked list.
    entry: UnsafeCell<Entry>,

    /// We are pinned to prevent the entry from being moved.
    _pinned: PhantomPinned,
}

#[cfg(feature = "std")]
impl UnwindSafe for EventListener<'_> {}
#[cfg(feature = "std")]
impl RefUnwindSafe for EventListener<'_> {}

unsafe impl Send for EventListener<'_> {}
unsafe impl Sync for EventListener<'_> {}

#[cfg(feature = "std")]
impl EventListener<'_> {
    /// Blocks until a notification is received.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // Notify `listener`.
    /// event.notify(1);
    ///
    /// // Receive the notification.
    /// listener.as_mut().wait();
    /// ```
    pub fn wait(self: Pin<&mut Self>) {
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
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(!listener.as_mut().wait_timeout(Duration::from_secs(1)));
    /// ```
    pub fn wait_timeout(self: Pin<&mut Self>, timeout: Duration) -> bool {
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
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(!listener.as_mut().wait_deadline(Instant::now() + Duration::from_secs(1)));
    /// ```
    pub fn wait_deadline(self: Pin<&mut Self>, deadline: Instant) -> bool {
        self.wait_internal(Some(deadline))
    }

    fn wait_internal(self: Pin<&mut Self>, deadline: Option<Instant>) -> bool {
        // Take out the inner pointer and set it to `None`.
        // SAFETY: Logically, the `inner` pointer is not pinned, so we can move it out.
        let this = unsafe { self.get_unchecked_mut() };

        let (parker, unparker) = parking::pair();
        let inner = match this.inner.take() {
            Some(inner) => inner,
            None => panic!("Cannot wait on a discarded listener"),
        };

        // Set this listener's state to `Waiting`.
        {
            let mut list = inner.lock();

            // Now that the list is locked, we can access the entry.
            let entry = unsafe { &mut *this.entry.get() };

            // If the listener was notified, we're done.
            match entry.state().replace(State::Notified(false)) {
                State::Notified(_) => {
                    unsafe {
                        list.remove(NonNull::new_unchecked(entry));
                    }
                    return true;
                }
                _ => entry.state().set(State::Task(Task::Thread(unparker))),
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
                        // Remove the entry and check if notified.
                        let mut list = inner.lock();
                        let state =
                            unsafe { list.remove(NonNull::new_unchecked(this.entry.get())) };
                        return state.is_notified();
                    }

                    // Park until the deadline.
                    parker.park_timeout(deadline - now);
                }
            }

            let mut list = inner.lock();

            // SAFETY: Now that the list is locked, we can access our entry.
            let entry = unsafe { &mut *this.entry.get() };

            // Do a dummy replace operation in order to take out the state.
            match entry.state().replace(State::Notified(false)) {
                State::Notified(_) => {
                    // If this listener has been notified, remove it from the list and return.
                    unsafe {
                        list.remove(NonNull::new_unchecked(entry));
                    }
                    return true;
                }
                // Otherwise, set the state back to `Waiting`.
                state => entry.state().set(state),
            }
        }
    }
}

impl<'a> EventListener<'a> {
    /// Creates a new, empty `EventListener` not associated with an event.
    ///
    /// To associate with an event, pin the `EventListener` and call [`EventListener::listen_to`].
    pub fn new() -> EventListener<'a> {
        EventListener {
            inner: None,
            entry: UnsafeCell::new(Entry::new()),
            _pinned: PhantomPinned,
        }
    }

    /// Make this `EventListener` listen on the given `Event`.
    pub fn listen_to(self: Pin<&mut Self>, event: &'a Event) {
        // SAFETY: We never move anything out of `self`.
        let this = unsafe { self.get_unchecked_mut() };

        // Get a reference to the `inner`.
        this.inner = Some(&event.inner);

        // Lock the list and add our entry.
        let mut list = event.inner.lock();
        unsafe {
            list.insert(NonNull::new_unchecked(this.entry.get()));
        }
    }

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
    /// let mut listener1 = event.listen();
    /// let mut listener2 = event.listen();
    ///
    /// event.notify(1);
    ///
    /// assert!(listener1.as_mut().discard());
    /// assert!(!listener2.as_mut().discard());
    /// ```
    pub fn discard(self: Pin<&mut Self>) -> bool {
        // SAFETY: We never move any pinned fields out of `self`.
        let this = unsafe { self.get_unchecked_mut() };

        // If this listener has never picked up a notification...
        if let Some(inner) = this.inner.take() {
            // Remove the listener from the list and return `true` if it was notified.
            let mut lock = inner.lock();
            unsafe {
                return lock
                    .remove(NonNull::new_unchecked(this.entry.get()))
                    .is_notified();
            }
        }

        false
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
        match self.inner {
            Some(inner) => ptr::eq(inner, &event.inner),
            None => false,
        }
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
    pub fn same_event(&self, other: &EventListener<'_>) -> bool {
        match (self.inner, other.inner) {
            (Some(inner), Some(other)) => ptr::eq(inner, other),
            _ => false,
        }
    }
}

impl fmt::Debug for EventListener<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Future for EventListener<'_> {
    type Output = ();

    #[allow(unreachable_patterns)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We only move fields that aren't `entry` out of `self`.
        let this = unsafe { self.get_unchecked_mut() };

        let inner = match this.inner {
            Some(inner) => inner,
            None => {
                panic!("cannot poll a completed `EventListener` future")
            }
        };
        let mut list = inner.lock();

        // SAFETY: Now that the list is locked, we can access our entry.
        let entry = unsafe { &mut *this.entry.get() };
        let state = entry.state();

        // Do a dummy replace operation in order to take out the state.
        match state.replace(State::Notified(false)) {
            State::Notified(_) => {
                // If this listener has been notified, remove it from the list and return.
                unsafe {
                    list.remove(NonNull::new_unchecked(entry));
                }
                drop(list);
                this.inner = None;
                return Poll::Ready(());
            }
            State::Created => {
                // If the listener was just created, put it in the `Polling` state.
                state.set(State::Task(Task::Waker(cx.waker().clone())));
            }
            State::Task(Task::Waker(w)) => {
                // If the listener was in the `Polling` state, update the waker.
                if w.will_wake(cx.waker()) {
                    state.set(State::Task(Task::Waker(w)));
                } else {
                    state.set(State::Task(Task::Waker(cx.waker().clone())));
                }
            }
            State::Task(_) => {
                unreachable!("cannot poll and wait on `EventListener` at the same time")
            }
        }

        Poll::Pending
    }
}

impl Default for EventListener<'_> {
    fn default() -> Self {
        EventListener::new()
    }
}

impl Drop for EventListener<'_> {
    fn drop(&mut self) {
        // SAFETY: We have implicitly called get_mut_unchecked here, so we can't move anything out of `self`,
        // aside from `inner`.

        // If this listener has never picked up a notification...
        if let Some(inner) = self.inner.take() {
            let mut list = inner.lock();
            // But if a notification was delivered to it...
            if let State::Notified(additional) =
                unsafe { list.remove(NonNull::new_unchecked(self.entry.get())) }
            {
                // Then pass it on to another active listener.
                list.notify(1, additional);
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
