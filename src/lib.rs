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

#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

extern crate alloc;

#[cfg_attr(feature = "std", path = "std.rs")]
#[cfg_attr(not(feature = "std"), path = "no_std.rs")]
mod sys;

use alloc::boxed::Box;

use core::borrow::Borrow;
use core::fmt;
use core::future::Future;
use core::marker::PhantomPinned;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll, Waker};

#[cfg(feature = "std")]
use parking::{Parker, Unparker};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

use sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use sync::{Arc, WithMut};

/// 1.39-compatible replacement for `matches!`
macro_rules! matches {
    ($expr:expr, $($pattern:pat)|+ $(if $guard: expr)?) => {
        match $expr {
            $($pattern)|+ $(if $guard)? => true,
            _ => false,
        }
    };
}

/// Inner state of [`Event`].
struct Inner {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    notified: AtomicUsize,

    /// Inner queue of event listeners.
    ///
    /// On `std` platforms, this is an intrusive linked list. On `no_std` platforms, this is a
    /// more traditional `Vec` of listeners, with an atomic queue used as a backup for high
    /// contention.
    list: sys::List,
}

impl Inner {
    fn new() -> Self {
        Self {
            notified: AtomicUsize::new(core::usize::MAX),
            list: sys::List::new(),
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

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

#[cfg(feature = "std")]
impl std::panic::UnwindSafe for Event {}
#[cfg(feature = "std")]
impl std::panic::RefUnwindSafe for Event {}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pad { .. }")
    }
}

impl Default for Event {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

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
    pub const fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Returns a guard listening for a notification.
    ///
    /// This method emits a `SeqCst` fence after registering a listener. For now, this method
    /// is an alias for calling [`EventListener::new()`], pinning it to the heap, and then
    /// inserting it into a list.
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
    pub fn listen(&self) -> Pin<Box<EventListener>> {
        let mut listener = Box::pin(EventListener::new(self));
        listener.as_mut().listen();
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
                inner.notify(n, true);
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
            if inner.notified.load(Ordering::Acquire) < core::usize::MAX {
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
            if inner.notified.load(Ordering::Acquire) < core::usize::MAX {
                inner.notify(n, true);
            }
        }
    }

    /// Return a reference to the inner state if it has been initialized.
    #[inline]
    fn try_inner(&self) -> Option<&Inner> {
        let inner = self.inner.load(Ordering::Acquire);
        unsafe { inner.as_ref() }
    }

    /// Returns a raw, initialized pointer to the inner state.
    ///
    /// This returns a raw pointer instead of reference because `from_raw`
    /// requires raw/mut provenance: <https://github.com/rust-lang/rust/pull/67339>.
    fn inner(&self) -> *const Inner {
        let mut inner = self.inner.load(Ordering::Acquire);

        // If this is the first use, initialize the state.
        if inner.is_null() {
            // Allocate the state on the heap.
            let new = Arc::new(Inner::new());

            // Convert the state to a raw pointer.
            let new = Arc::into_raw(new) as *mut Inner;

            // Replace the null pointer with the new state pointer.
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
        self.inner.with_mut(|&mut inner| {
            // If the state pointer has been initialized, drop it.
            if !inner.is_null() {
                unsafe {
                    drop(Arc::from_raw(inner));
                }
            }
        })
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
pub struct EventListener(Listener<Arc<Inner>>);

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventListener { .. }")
    }
}

impl EventListener {
    /// Create a new `EventListener` that will wait for a notification from the given [`Event`].
    pub fn new(event: &Event) -> Self {
        let inner = event.inner();

        let listener = Listener {
            event: unsafe { Arc::clone(&ManuallyDrop::new(Arc::from_raw(inner))) },
            listener: None,
            _pin: PhantomPinned,
        };

        Self(listener)
    }

    /// Register this listener into the given [`Event`].
    ///
    /// This method can only be called after the listener has been pinned, and must be called before
    /// the listener is polled.
    pub fn listen(self: Pin<&mut Self>) {
        self.listener().insert();

        // Make sure the listener is registered before whatever happens next.
        full_fence();
    }

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
    #[cfg(feature = "std")]
    pub fn wait(self: Pin<&mut Self>) {
        self.listener().wait_internal(None);
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
    #[cfg(feature = "std")]
    pub fn wait_timeout(self: Pin<&mut Self>, timeout: Duration) -> bool {
        self.listener()
            .wait_internal(Instant::now().checked_add(timeout))
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
    #[cfg(feature = "std")]
    pub fn wait_deadline(self: Pin<&mut Self>, deadline: Instant) -> bool {
        self.listener().wait_internal(Some(deadline))
    }

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
    /// let mut listener1 = event.listen();
    /// let mut listener2 = event.listen();
    ///
    /// event.notify(1);
    ///
    /// assert!(listener1.as_mut().discard());
    /// assert!(!listener2.as_mut().discard());
    /// ```
    pub fn discard(self: Pin<&mut Self>) -> bool {
        self.listener().discard()
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
        ptr::eq::<Inner>(&**self.inner(), event.inner.load(Ordering::Acquire))
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
        ptr::eq::<Inner>(&**self.inner(), &**other.inner())
    }

    fn listener(self: Pin<&mut Self>) -> Pin<&mut Listener<Arc<Inner>>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.0.event
    }
}

impl Future for EventListener {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.listener().poll_internal(cx)
    }
}

struct Listener<B: Borrow<Inner> + Unpin> {
    /// The reference to the original event.
    event: B,

    /// The inner state of the listener.
    listener: Option<sys::Listener>,

    /// Enforce pinning.
    _pin: PhantomPinned,
}

unsafe impl<B: Borrow<Inner> + Unpin + Send> Send for Listener<B> {}
unsafe impl<B: Borrow<Inner> + Unpin + Sync> Sync for Listener<B> {}

impl<B: Borrow<Inner> + Unpin> Listener<B> {
    /// Pin-project this listener.
    fn project(self: Pin<&mut Self>) -> (&Inner, Pin<&mut Option<sys::Listener>>) {
        // SAFETY: `event` is `Unpin`, and `listener`'s pin status is preserved
        unsafe {
            let Listener {
                event, listener, ..
            } = self.get_unchecked_mut();

            ((*event).borrow(), Pin::new_unchecked(listener))
        }
    }

    /// Register this listener with the event.
    fn insert(self: Pin<&mut Self>) {
        let (inner, listener) = self.project();
        inner.insert(listener);
    }

    /// Wait until the provided deadline.
    #[cfg(feature = "std")]
    fn wait_internal(mut self: Pin<&mut Self>, deadline: Option<Instant>) -> bool {
        use std::cell::RefCell;

        std::thread_local! {
            /// Cached thread-local parker/unparker pair.
            static PARKER: RefCell<Option<(Parker, Task)>> = RefCell::new(None);
        }

        // Try to borrow the thread-local parker/unparker pair.
        PARKER
            .try_with({
                let this = self.as_mut();
                |parker| {
                    let mut pair = parker
                        .try_borrow_mut()
                        .expect("Shouldn't be able to borrow parker reentrantly");
                    let (parker, unparker) = pair.get_or_insert_with(|| {
                        let (parker, unparker) = parking::pair();
                        (parker, Task::Unparker(unparker))
                    });

                    this.wait_with_parker(deadline, parker, unparker.as_task_ref())
                }
            })
            .unwrap_or_else(|_| {
                // If the pair isn't accessible, we may be being called in a destructor.
                // Just create a new pair.
                let (parker, unparker) = parking::pair();
                self.wait_with_parker(deadline, &parker, TaskRef::Unparker(&unparker))
            })
    }

    /// Wait until the provided deadline using the specified parker/unparker pair.
    #[cfg(feature = "std")]
    fn wait_with_parker(
        self: Pin<&mut Self>,
        deadline: Option<Instant>,
        parker: &Parker,
        unparker: TaskRef<'_>,
    ) -> bool {
        let (inner, mut listener) = self.project();

        // Set the listener's state to `Task`.
        match inner.register(listener.as_mut(), unparker) {
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
                    // Make sure we're not timed out already.
                    let now = Instant::now();
                    if now >= deadline {
                        // Remove our entry and check if we were notified.
                        return inner
                            .remove(listener, false)
                            .expect("We never removed ourself from the list")
                            .is_notified();
                    }
                }
            }

            // See if we were notified.
            if inner
                .register(listener.as_mut(), unparker)
                .expect("We never removed ourself from the list")
            {
                return true;
            }
        }
    }

    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    fn discard(self: Pin<&mut Self>) -> bool {
        let (inner, listener) = self.project();

        inner
            .remove(listener, false)
            .map_or(false, |state| state.is_notified())
    }

    /// Poll this listener for a notification.
    fn poll_internal(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let (inner, mut listener) = self.project();

        // Try to register the listener.
        match inner.register(listener.as_mut(), TaskRef::Waker(cx.waker())) {
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

impl<B: Borrow<Inner> + Unpin> Drop for Listener<B> {
    fn drop(&mut self) {
        // If we're being dropped, we need to remove ourself from the list.
        let (inner, listener) = unsafe { Pin::new_unchecked(self).project() };

        inner.remove(listener, true);
    }
}

/// The state of a listener.
#[derive(Debug, PartialEq)]
enum State {
    /// The listener was just created.
    Created,

    /// The listener has received a notification.
    ///
    /// The `bool` is `true` if this was an "additional" notification.
    Notified(bool),

    /// A task is waiting for a notification.
    Task(Task),

    /// Empty hole used to replace a notified listener.
    NotifiedTaken,
}

impl State {
    fn is_notified(&self) -> bool {
        matches!(self, Self::Notified(_) | Self::NotifiedTaken)
    }
}

/// A task that can be woken up.
#[derive(Debug, Clone)]
enum Task {
    /// A waker that wakes up a future.
    Waker(Waker),

    /// An unparker that wakes up a thread.
    #[cfg(feature = "std")]
    Unparker(Unparker),
}

impl Task {
    fn as_task_ref(&self) -> TaskRef<'_> {
        match self {
            Self::Waker(waker) => TaskRef::Waker(waker),
            #[cfg(feature = "std")]
            Self::Unparker(unparker) => TaskRef::Unparker(unparker),
        }
    }

    fn wake(self) {
        match self {
            Self::Waker(waker) => waker.wake(),
            #[cfg(feature = "std")]
            Self::Unparker(unparker) => {
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
#[derive(Clone, Copy)]
enum TaskRef<'a> {
    /// A waker that wakes up a future.
    Waker(&'a Waker),

    /// An unparker that wakes up a thread.
    #[cfg(feature = "std")]
    Unparker(&'a Unparker),
}

impl TaskRef<'_> {
    /// Tells if this task will wake up the other task.
    #[allow(unreachable_patterns)]
    fn will_wake(self, other: Self) -> bool {
        match (self, other) {
            (Self::Waker(a), Self::Waker(b)) => a.will_wake(b),
            #[cfg(feature = "std")]
            (Self::Unparker(_), Self::Unparker(_)) => {
                // TODO: Use unreleased will_unpark API.
                false
            }
            _ => false,
        }
    }

    /// Converts this task reference to a task by cloning.
    fn into_task(self) -> Task {
        match self {
            Self::Waker(waker) => Task::Waker(waker.clone()),
            #[cfg(feature = "std")]
            Self::Unparker(unparker) => Task::Unparker(unparker.clone()),
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
        sync::atomic::compiler_fence(Ordering::SeqCst);
        let a = AtomicUsize::new(0);
        let _ = a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        sync::atomic::compiler_fence(Ordering::SeqCst);
    } else {
        sync::atomic::fence(Ordering::SeqCst);
    }
}

/// Synchronization primitive implementation.
mod sync {
    pub(super) use alloc::sync::Arc;
    pub(super) use core::cell;
    pub(super) use core::sync::atomic;

    #[cfg(feature = "std")]
    pub(super) use std::sync::{Mutex, MutexGuard};

    pub(super) trait WithMut {
        type Output;

        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Output) -> R;
    }

    impl<T> WithMut for atomic::AtomicPtr<T> {
        type Output = *mut T;

        #[inline]
        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Output) -> R,
        {
            f(self.get_mut())
        }
    }
}
