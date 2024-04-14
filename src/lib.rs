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
//! use event_listener::{Event, Listener};
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
//!
//! # Features
//!
//! - The `portable-atomic` feature enables the use of the [`portable-atomic`] crate to provide
//!   atomic operations on platforms that don't support them.
//!
//! [`portable-atomic`]: https://crates.io/crates/portable-atomic

#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use loom::atomic::{self, AtomicPtr, AtomicUsize, Ordering};
use loom::Arc;
use notify::{GenericNotify, Internal, NotificationPrivate};

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::future::Future;
use std::mem::{self, ManuallyDrop};
use std::ops::{Deref, DerefMut};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};
use std::usize;

mod notify;
pub use notify::{IntoNotification, Notification};

/// Create a stack-based event listener for an [`Event`].
///
/// [`EventListener`] allocates the listener on the heap. While this works for most use cases, in
/// practice this heap allocation can be expensive for repeated uses. This method allows for
/// allocating the listener on the stack instead.
///
/// There are limitations to using this macro instead of the [`EventListener`] type, however.
/// Firstly, it is significantly less flexible. The listener is locked to the current stack
/// frame, meaning that it can't be returned or put into a place where it would go out of
/// scope. For instance, this will not work:
///
/// ```
/// use event_listener::{Event, Listener, listener};
///
/// fn get_listener(event: &Event) -> impl Listener {
///     listener!(event => cant_return_this);
///     cant_return_this
/// }
/// ```
///
/// In addition, the types involved in creating this listener are not able to be named. Therefore
/// it cannot be used in hand-rolled futures or similar structures.
///
/// The type created by this macro implements [`Listener`], allowing it to be used in cases where
/// [`EventListener`] would normally be used.
///
/// ## Example
///
/// To use this macro, replace cases where you would normally use this...
///
/// ```no_compile
/// let listener = event.listen();
/// ```
///
/// ...with this:
///
/// ```no_compile
/// listener!(event => listener);
/// ```
///
/// Here is the top level example from this crate's documentation, but using [`listener`] instead
/// of [`EventListener`].
///
/// ```
/// use std::sync::atomic::{AtomicBool, Ordering};
/// use std::sync::Arc;
/// use std::thread;
/// use std::time::Duration;
/// use std::usize;
/// use event_listener::{Event, listener, IntoNotification, Listener};
///
/// let flag = Arc::new(AtomicBool::new(false));
/// let event = Arc::new(Event::new());
///
/// // Spawn a thread that will set the flag after 1 second.
/// thread::spawn({
///     let flag = flag.clone();
///     let event = event.clone();
///     move || {
///         // Wait for a second.
///         thread::sleep(Duration::from_secs(1));
///
///         // Set the flag.
///         flag.store(true, Ordering::SeqCst);
///
///         // Notify all listeners that the flag has been set.
///         event.notify(usize::MAX);
///     }
/// });
///
/// // Wait until the flag is set.
/// loop {
///     // Check the flag.
///     if flag.load(Ordering::SeqCst) {
///         break;
///     }
///
///     // Start listening for events.
///     // NEW: Changed to a stack-based listener.
///     listener!(event => listener);
///
///     // Check the flag again after creating the listener.
///     if flag.load(Ordering::SeqCst) {
///         break;
///     }
///
///     // Wait for a notification and continue the loop.
///     listener.wait();
/// }
/// ```
#[macro_export]
macro_rules! listener {
    ($event:expr => $listener:ident) => {
        // TODO: Stack allocation.
        let mut $listener = ($event).listen();
    };
}

/// Inner state of [`Event`].
struct Inner<T> {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    notified: AtomicUsize,

    /// A linked list holding registered listeners.
    list: Mutex<List<T>>,

    /// A single cached list entry to avoid allocations on the fast path of the insertion.
    cache: UnsafeCell<Entry<T>>,
}

impl<T> Inner<T> {
    /// Locks the list.
    fn lock(&self) -> ListGuard<'_, T> {
        ListGuard {
            inner: self,
            guard: self.list.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// Returns the pointer to the single cached list entry.
    #[inline(always)]
    fn cache_ptr(&self) -> NonNull<Entry<T>> {
        unsafe { NonNull::new_unchecked(self.cache.get()) }
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
pub struct Event<T = ()> {
    /// A pointer to heap-allocated inner state.
    ///
    /// This pointer is initially null and gets lazily initialized on first use. Semantically, it
    /// is an `Arc<Inner>` so it's important to keep in mind that it contributes to the [`Arc`]'s
    /// reference count.
    inner: AtomicPtr<Inner<T>>,
}

unsafe impl<T: Send> Send for Event<T> {}
unsafe impl<T: Send> Sync for Event<T> {}

impl<T> UnwindSafe for Event<T> {}
impl<T> RefUnwindSafe for Event<T> {}

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
        Self::with_tag()
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
    pub fn notify_relaxed(&self, n: usize) -> usize {
        self.notify(n.relaxed())
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
    pub fn notify_additional(&self, n: usize) -> usize {
        self.notify(n.additional())
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
    pub fn notify_additional_relaxed(&self, n: usize) -> usize {
        self.notify(n.additional().relaxed())
    }
}

impl<T> Event<T> {
    /// Creates a new [`Event`] with a tag.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::<usize>::with_tag();
    /// ```
    #[inline]
    pub const fn with_tag() -> Event<T> {
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
    pub fn listen(&self) -> EventListener<T> {
        let inner = self.inner();
        let listener = EventListener {
            inner: unsafe { Arc::clone(&ManuallyDrop::new(Arc::from_raw(inner))) },
            entry: unsafe { Some((*inner).lock().insert((*inner).cache_ptr())) },
        };

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
    pub fn notify(&self, notify: impl IntoNotification<Tag = T>) -> usize {
        let notify = notify.into_notification();

        // Make sure the notification comes after whatever triggered it.
        notify.fence(Internal::new());

        if let Some(inner) = self.try_inner() {
            let limit = if notify.is_additional(Internal::new()) {
                // Notify if there is at least one unnotified listener.
                std::usize::MAX
            } else {
                // Notify if there is at least one unnotified listener and the number of notified
                // listeners is less than `n`.
                notify.count(Internal::new())
            };

            if inner.notified.load(Ordering::Acquire) < limit {
                return inner.lock().notify(notify);
            }
        }

        0
    }

    /// Returns a reference to the inner state if it was initialized.
    #[inline]
    fn try_inner(&self) -> Option<&Inner<T>> {
        let inner = self.inner.load(Ordering::Acquire);
        unsafe { inner.as_ref() }
    }

    /// Returns a raw pointer to the inner state, initializing it if necessary.
    ///
    /// This returns a raw pointer instead of reference because `from_raw`
    /// requires raw/mut provenance: <https://github.com/rust-lang/rust/pull/67339>
    fn inner(&self) -> *const Inner<T> {
        let mut inner = self.inner.load(Ordering::Acquire);

        // Initialize the state if this is its first use.
        if inner.is_null() {
            // Allocate on the heap.
            let new = Arc::new(Inner {
                notified: AtomicUsize::new(usize::MAX),
                list: std::sync::Mutex::new(List::<T> {
                    head: None,
                    tail: None,
                    start: None,
                    len: 0,
                    notified: 0,
                    cache_used: false,
                }),
                cache: UnsafeCell::new(Entry {
                    state: Cell::new(State::Created),
                    prev: Cell::new(None),
                    next: Cell::new(None),
                }),
            });
            // Convert the heap-allocated state into a raw pointer.
            let new = Arc::into_raw(new) as *mut Inner<T>;

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

impl<T> Drop for Event<T> {
    #[inline]
    fn drop(&mut self) {
        let inner: *mut Inner<T> = *self.inner.get_mut();

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

/// A handle that is listening to an [`Event`].
///
/// This trait represents a type waiting for a notification from an [`Event`]. See the
/// [`EventListener`] type for more documentation on this trait's usage.
pub trait Listener<T = ()>: Future<Output = T> + __sealed::Sealed {
    /// Blocks until a notification is received.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // Notify `listener`.
    /// event.notify(1);
    ///
    /// // Receive the notification.
    /// listener.wait();
    /// ```
    fn wait(self) -> T;

    /// Blocks until a notification is received or a timeout is reached.
    ///
    /// Returns `true` if a notification was received.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(listener.wait_timeout(Duration::from_secs(1)).is_none());
    /// ```
    fn wait_timeout(self, timeout: Duration) -> Option<T>;

    /// Blocks until a notification is received or a deadline is reached.
    ///
    /// Returns `true` if a notification was received.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(listener.wait_deadline(Instant::now() + Duration::from_secs(1)).is_none());
    /// ```
    fn wait_deadline(self, deadline: Instant) -> Option<T>;

    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    ///
    /// Returns `true` if a notification was discarded.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener1 = event.listen();
    /// let mut listener2 = event.listen();
    ///
    /// event.notify(1);
    ///
    /// assert!(listener1.discard());
    /// assert!(!listener2.discard());
    /// ```
    fn discard(self) -> bool;

    /// Returns `true` if this listener listens to the given `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// assert!(listener.listens_to(&event));
    /// ```
    fn listens_to(&self, event: &Event<T>) -> bool;

    /// Returns `true` if both listeners listen to the same `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// assert!(listener1.same_event(&listener2));
    /// ```
    fn same_event(&self, other: &Self) -> bool;
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
pub struct EventListener<T = ()> {
    /// A reference to [`Event`]'s inner state.
    inner: Arc<Inner<T>>,

    /// A pointer to this listener's entry in the linked list.
    entry: Option<NonNull<Entry<T>>>,
}

unsafe impl<T: Send> Send for EventListener<T> {}
unsafe impl<T: Send> Sync for EventListener<T> {}

impl<T> UnwindSafe for EventListener<T> {}
impl<T> RefUnwindSafe for EventListener<T> {}
impl<T> __sealed::Sealed for EventListener<T> {}

impl<T> Listener<T> for EventListener<T> {
    fn wait(self) -> T {
        self.wait_internal(None).unwrap()
    }

    fn wait_deadline(self, deadline: Instant) -> Option<T> {
        self.wait_internal(Some(deadline))
    }

    fn wait_timeout(self, timeout: Duration) -> Option<T> {
        self.wait_internal(Instant::now().checked_add(timeout))
    }

    fn discard(mut self) -> bool {
        // If this listener has never picked up a notification...
        if let Some(entry) = self.entry.take() {
            let mut list = self.inner.lock();
            // Remove the listener from the list and return `true` if it was notified.
            if list.remove(entry, self.inner.cache_ptr()).is_notified() {
                return true;
            }
        }
        false
    }

    fn listens_to(&self, event: &Event<T>) -> bool {
        ptr::eq::<Inner<T>>(&*self.inner, event.inner.load(Ordering::Acquire))
    }

    fn same_event(&self, other: &Self) -> bool {
        ptr::eq::<Inner<T>>(&*self.inner, &*other.inner)
    }
}

impl<T> EventListener<T> {
    fn wait_internal(mut self, deadline: Option<Instant>) -> Option<T> {
        // Take out the entry pointer and set it to `None`.
        let entry = match self.entry.take() {
            None => unreachable!("cannot wait twice on an `EventListener`"),
            Some(entry) => entry,
        };

        // Set this listener's state to `Waiting`.
        {
            let mut list = self.inner.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match take_state(&e.state) {
                State::Notified { .. } => {
                    // If this listener has been notified, remove it from the list and return.
                    return list.remove(entry, self.inner.cache_ptr()).take();
                }
                // Otherwise, set the state to `Waiting`.
                _ => e.state.set(State::Waiting(thread::current())),
            }
        }

        // Wait until a notification is received or the timeout is reached.
        loop {
            match deadline {
                None => thread::park(),

                Some(deadline) => {
                    // Check for timeout.
                    let now = Instant::now();
                    if now >= deadline {
                        // Remove the entry and check if notified.
                        return self
                            .inner
                            .lock()
                            .remove(entry, self.inner.cache_ptr())
                            .take();
                    }

                    // Park until the deadline.
                    thread::park_timeout(deadline - now);
                }
            }

            let mut list = self.inner.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match take_state(&e.state) {
                State::Notified { .. } => {
                    // If this listener has been notified, remove it from the list and return.
                    return list.remove(entry, self.inner.cache_ptr()).take();
                }
                // Otherwise, set the state back to `Waiting`.
                state => e.state.set(state),
            }
        }
    }
}

impl<T> fmt::Debug for EventListener<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl<T> Future for EventListener<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut list = self.inner.lock();

        let entry = match self.entry {
            None => unreachable!("cannot poll a completed `EventListener` future"),
            Some(entry) => entry,
        };
        let state = unsafe { &entry.as_ref().state };

        // Do a dummy replace operation in order to take out the state.
        match take_state(state) {
            State::Notified { .. } => {
                // If this listener has been notified, remove it from the list and return.
                let state = list.remove(entry, self.inner.cache_ptr());
                drop(list);
                self.entry = None;

                match state {
                    State::Notified { tag: Some(tag), .. } => return Poll::Ready(tag),
                    _ => unreachable!(),
                }
            }
            State::Created => {
                // If the listener was just created, put it in the `Polling` state.
                state.set(State::Polling(cx.waker().clone()));
            }
            State::Polling(w) => {
                // If the listener was in the `Polling` state, update the waker.
                if w.will_wake(cx.waker()) {
                    state.set(State::Polling(w));
                } else {
                    state.set(State::Polling(cx.waker().clone()));
                }
            }
            State::Waiting(_) => {
                unreachable!("cannot poll and wait on `EventListener` at the same time")
            }
        }

        Poll::Pending
    }
}

impl<T> Drop for EventListener<T> {
    fn drop(&mut self) {
        // If this listener has never picked up a notification...
        if let Some(entry) = self.entry.take() {
            let mut list = self.inner.lock();

            // But if a notification was delivered to it...
            if let State::Notified {
                additional,
                tag: Some(tag),
            } = list.remove(entry, self.inner.cache_ptr())
            {
                let mut tag = Some(tag);

                // Then pass it on to another active listener.
                list.notify(GenericNotify::new(1, additional, || tag.take().unwrap()));
            }
        }
    }
}

/// A guard holding the linked list locked.
struct ListGuard<'a, T> {
    /// A reference to [`Event`]'s inner state.
    inner: &'a Inner<T>,

    /// The actual guard that acquired the linked list.
    guard: MutexGuard<'a, List<T>>,
}

impl<T> Drop for ListGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        let list = &mut **self;

        // Update the atomic `notified` counter.
        let notified = if list.notified < list.len {
            list.notified
        } else {
            usize::MAX
        };
        self.inner.notified.store(notified, Ordering::Release);
    }
}

impl<T> Deref for ListGuard<'_, T> {
    type Target = List<T>;

    #[inline]
    fn deref(&self) -> &List<T> {
        &self.guard
    }
}

impl<T> DerefMut for ListGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut List<T> {
        &mut self.guard
    }
}

/// The state of a listener.
enum State<T> {
    /// It has just been created.
    Created,

    /// It has received a notification.
    Notified {
        /// This is `true` if this was an "additional" notification.
        additional: bool,

        /// The tag associated with this event.
        tag: Option<T>,
    },

    /// An async task is polling it.
    Polling(Waker),

    /// A thread is blocked on it.
    Waiting(Thread),
}

impl<T> State<T> {
    /// Returns `true` if this is the `Notified` state.
    #[inline]
    fn is_notified(&self) -> bool {
        match self {
            State::Notified { .. } => true,
            State::Created | State::Polling(_) | State::Waiting(_) => false,
        }
    }

    /// Take the tag out if it exists.
    #[inline]
    fn take(&mut self) -> Option<T> {
        match self {
            State::Notified { tag, .. } => Some(tag.take().expect("tag taken twice")),
            _ => None,
        }
    }
}

/// An entry representing a registered listener.
struct Entry<T> {
    /// THe state of this listener.
    state: Cell<State<T>>,

    /// Previous entry in the linked list.
    prev: Cell<Option<NonNull<Entry<T>>>>,

    /// Next entry in the linked list.
    next: Cell<Option<NonNull<Entry<T>>>>,
}

/// A linked list of entries.
struct List<T> {
    /// First entry in the list.
    head: Option<NonNull<Entry<T>>>,

    /// Last entry in the list.
    tail: Option<NonNull<Entry<T>>>,

    /// The first unnotified entry in the list.
    start: Option<NonNull<Entry<T>>>,

    /// Total number of entries in the list.
    len: usize,

    /// The number of notified entries in the list.
    notified: usize,

    /// Whether the cached entry is used.
    cache_used: bool,
}

impl<T> List<T> {
    /// Inserts a new entry into the list.
    fn insert(&mut self, cache: NonNull<Entry<T>>) -> NonNull<Entry<T>> {
        unsafe {
            let entry = Entry {
                state: Cell::new(State::Created),
                prev: Cell::new(self.tail),
                next: Cell::new(None),
            };

            let entry = if self.cache_used {
                // Allocate an entry that is going to become the new tail.
                NonNull::new_unchecked(Box::into_raw(Box::new(entry)))
            } else {
                // No need to allocate - we can use the cached entry.
                self.cache_used = true;
                cache.as_ptr().write(entry);
                cache
            };

            // Replace the tail with the new entry.
            match mem::replace(&mut self.tail, Some(entry)) {
                None => self.head = Some(entry),
                Some(t) => t.as_ref().next.set(Some(entry)),
            }

            // If there were no unnotified entries, this one is the first now.
            if self.start.is_none() {
                self.start = self.tail;
            }

            // Bump the entry count.
            self.len += 1;

            entry
        }
    }

    /// Removes an entry from the list and returns its state.
    fn remove(&mut self, entry: NonNull<Entry<T>>, cache: NonNull<Entry<T>>) -> State<T> {
        unsafe {
            let prev = entry.as_ref().prev.get();
            let next = entry.as_ref().next.get();

            // Unlink from the previous entry.
            match prev {
                None => self.head = next,
                Some(p) => p.as_ref().next.set(next),
            }

            // Unlink from the next entry.
            match next {
                None => self.tail = prev,
                Some(n) => n.as_ref().prev.set(prev),
            }

            // If this was the first unnotified entry, move the pointer to the next one.
            if self.start == Some(entry) {
                self.start = next;
            }

            // Extract the state.
            let state = if ptr::eq(entry.as_ptr(), cache.as_ptr()) {
                // Free the cached entry.
                self.cache_used = false;
                entry.as_ref().state.replace(State::Created)
            } else {
                // Deallocate the entry.
                Box::from_raw(entry.as_ptr()).state.into_inner()
            };

            // Update the counters.
            if state.is_notified() {
                self.notified -= 1;
            }
            self.len -= 1;

            state
        }
    }

    /// Notifies a number of entries.
    #[cold]
    fn notify(&mut self, mut notify: impl Notification<Tag = T>) -> usize {
        let mut n = notify.count(Internal::new());

        if !notify.is_additional(Internal::new()) {
            if n <= self.notified {
                return 0;
            }
            n -= self.notified;
        }

        let count = n;

        while n > 0 {
            n -= 1;

            // Notify the first unnotified entry.
            match self.start {
                None => return count - n - 1,
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = unsafe { e.as_ref() };
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    match e.state.replace(State::Notified {
                        additional: notify.is_additional(Internal::new()),
                        tag: Some(notify.next_tag(Internal::new())),
                    }) {
                        State::Notified { .. } => {}
                        State::Created => {}
                        State::Polling(w) => w.wake(),
                        State::Waiting(t) => t.unpark(),
                    }

                    // Update the counter.
                    self.notified += 1;
                }
            }
        }

        count - n
    }
}

/// Take the `State` out of the slot without moving out the tag.
#[inline]
fn take_state<T>(state: &Cell<State<T>>) -> State<T> {
    match state.replace(State::Notified {
        additional: false,
        tag: None,
    }) {
        State::Notified { additional, tag } => {
            // Replace the tag so remove() can take it out.
            state.replace(State::Notified { additional, tag })
        }

        state => state,
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

mod loom {
    #[cfg(not(feature = "portable-atomic"))]
    pub(crate) use std::sync::atomic;

    #[cfg(not(feature = "portable-atomic"))]
    pub(crate) use std::sync::Arc;

    #[cfg(feature = "portable-atomic")]
    pub(crate) use portable_atomic_crate as atomic;

    #[cfg(feature = "portable-atomic")]
    pub(crate) use portable_atomic_util::Arc;
}

mod __sealed {
    #[doc(hidden)]
    pub trait Sealed {}
}
