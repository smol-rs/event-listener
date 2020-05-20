//! Notify async tasks or threads.
//!
//! This is a synchronization primitive similar to [eventcounts] invented by Dmitry Vyukov.
//!
//! You can use this crate to turn non-blocking data structures into async or blocking data
//! structures. See a [simple mutex] implementation that exposes an async and a blocking interface
//! for acquiring locks.
//!
//! [eventcounts]: http://www.1024cores.net/home/lock-free-algorithms/eventcounts
//! [simple mutex]: https://github.com/stjepang/event-listener/blob/master/examples/mutex.rs
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
//!         event.notify_all();
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

use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::mem::{self, ManuallyDrop};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

/// A bit set inside [`Event`] when there is at least one listener that has already been notified.
const NOTIFIED: usize = 1 << 0;

/// A bit set inside [`Event`] when there is at least one notifiable listener.
const NOTIFIABLE: usize = 1 << 1;

/// Inner state of [`Event`].
struct Inner {
    /// Holds bits [`NOTIFIED`] and [`NOTIFIABLE`].
    flags: AtomicUsize,

    /// A linked list holding registered listeners.
    list: Mutex<List>,
}

impl Inner {
    /// Locks the list.
    fn lock(&self) -> ListGuard<'_> {
        ListGuard {
            inner: self,
            guard: self.list.lock().unwrap(),
        }
    }
}

/// A synchronization primitive for notifying async tasks and threads.
///
/// Listeners can be registered using [`Event::listen()`]. There are two ways of notifying
/// listeners:
///
/// 1. [`Event::notify_one()`] notifies one listener.
/// 2. [`Event::notify_all()`] notifies all listeners.
///
/// If there are no active listeners at the time a notification is sent, it simply gets lost.
///
/// Note that [`Event::notify_one()`] does not notify one *additional* listener - it only makes
/// sure *at least* one listener among the active ones is notified.
///
/// There are two ways for a listener to wait for a notification:
///
/// 1. In an asynchronous manner using `.await`.
/// 2. In a blocking manner by calling [`EventListener::wait()`] on it.
///
/// If a notified listener is dropped without receiving a notification, dropping will notify
/// another active listener.
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
    pub fn new() -> Event {
        Event {
            inner: AtomicPtr::default(),
        }
    }

    /// Returns a guard listening for a notification.
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
        let listener = EventListener {
            inner: unsafe { Arc::clone(&ManuallyDrop::new(Arc::from_raw(inner))) },
            entry: Some(inner.lock().insert()),
        };

        // Make sure the listener is registered before whatever happens next.
        full_fence();
        listener
    }

    /// Notifies a single active listener.
    ///
    /// Note that this does not notify one *additional* listener - it only makes sure *at least*
    /// one listener among the active ones is notified.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify_one();
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// // Notifies just one of `listener1` and `listener2`.
    /// //
    /// // Listener queueing is fair, which means `listener1` gets notified
    /// // here since it was the first to start listening.
    /// event.notify_one();
    /// ```
    #[inline]
    pub fn notify_one(&self) {
        let inner = self.inner();

        // Make sure the notification comes after whatever triggered it.
        full_fence();

        // Notify if no active listeners have been notified and there is at least one listener.
        let flags = inner.flags.load(Ordering::Relaxed);
        if flags & NOTIFIED == 0 && flags & NOTIFIABLE != 0 {
            inner.lock().notify(false);
        }
    }

    /// Notifies all active listeners.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify_all();
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// // Both `listener1` and `listener2` get notified.
    /// event.notify_all();
    /// ```
    #[inline]
    pub fn notify_all(&self) {
        let inner = self.inner();

        // Make sure the notification comes after whatever triggered it.
        full_fence();

        // Notify if there is at least one listener.
        if inner.flags.load(Ordering::Relaxed) & NOTIFIABLE != 0 {
            inner.lock().notify(true);
        }
    }

    /// Returns a reference to the inner state.
    fn inner(&self) -> &Inner {
        let mut inner = self.inner.load(Ordering::Acquire);

        // Initialize the state if this is its first use.
        if inner.is_null() {
            // Allocate on the heap.
            let new = Arc::new(Inner {
                flags: AtomicUsize::new(0),
                list: Mutex::new(List {
                    head: None,
                    tail: None,
                    len: 0,
                    notifiable: 0,
                }),
            });
            // Convert the heap-allocated state into a raw pointer.
            let new = Arc::into_raw(new) as *mut Inner;

            // Attempt to replace the null-pointer with the new state pointer.
            inner = self.inner.compare_and_swap(inner, new, Ordering::AcqRel);

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

        unsafe { &*inner }
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
/// another active listener.
pub struct EventListener {
    /// A reference to [`Event`]'s inner state.
    inner: Arc<Inner>,

    /// A pointer to this listener's entry in the linked list.
    entry: Option<NonNull<Entry>>,
}

unsafe impl Send for EventListener {}
unsafe impl Sync for EventListener {}

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
    /// event.notify_one();
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

        // Set this listener's state to `Waiting`.
        {
            let mut list = self.inner.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match e.state.replace(State::Notified) {
                State::Notified => {
                    // If this listener has been notified, remove it from the list and return.
                    list.remove(entry);
                    return true;
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
                        return false;
                    }

                    // Park until the deadline.
                    thread::park_timeout(deadline - now);
                }
            }

            let mut list = self.inner.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match e.state.replace(State::Notified) {
                State::Notified => {
                    // If this listener has been notified, remove it from the list and return.
                    list.remove(entry);
                    return true;
                }
                // Otherwise, set the state back to `Waiting`.
                state => e.state.set(state),
            }
        }
    }
}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Future for EventListener {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut list = self.inner.lock();

        let entry = match self.entry {
            None => unreachable!("cannot poll a completed `EventListener` future"),
            Some(entry) => entry,
        };
        let state = unsafe { &entry.as_ref().state };

        // Do a dummy replace operation in order to take out the state.
        match state.replace(State::Notified) {
            State::Notified => {
                // If this listener has been notified, remove it from the list and return.
                list.remove(entry);
                drop(list);
                self.entry = None;
                return Poll::Ready(());
            }
            State::Created => {
                // If the listener was just created, put it in the `Polling` state.
                state.set(State::Polling(cx.waker().clone()));
            }
            State::Polling(w) => {
                // If the listener was in the `Pooling` state, keep it.
                state.set(State::Polling(w));
            }
            State::Waiting(_) => {
                unreachable!("cannot poll and wait on `EventListener` at the same time")
            }
        }

        Poll::Pending
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        // If this listener has never picked up a notification...
        if let Some(entry) = self.entry.take() {
            let mut list = self.inner.lock();

            // But if a notification was delivered to it...
            if list.remove(entry).is_notified() {
                // Then pass it on to another active listener.
                list.notify(false);
            }
        }
    }
}

/// A guard holding the linked list locked.
struct ListGuard<'a> {
    /// A reference to [`Event`]'s inner state.
    inner: &'a Inner,

    /// The actual guard that acquired the linked list.
    guard: MutexGuard<'a, List>,
}

impl Drop for ListGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        let list = &mut **self;
        let mut flags = 0;

        // Set the `NOTIFIED` flag if there is at least one notified listener.
        if list.len - list.notifiable > 0 {
            flags |= NOTIFIED;
        }

        // Set the `NOTIFIABLE` flag if there is at least one notifiable listener.
        if list.notifiable > 0 {
            flags |= NOTIFIABLE;
        }

        self.inner.flags.store(flags, Ordering::Release);
    }
}

impl Deref for ListGuard<'_> {
    type Target = List;

    #[inline]
    fn deref(&self) -> &List {
        &*self.guard
    }
}

impl DerefMut for ListGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut List {
        &mut *self.guard
    }
}

/// The state of a listener.
enum State {
    /// It has just been created.
    Created,

    /// It has received a notification.
    Notified,

    /// An async task is polling it.
    Polling(Waker),

    /// A thread is blocked on it.
    Waiting(Thread),
}

impl State {
    /// Returns `true` if this is the `Notified` state.
    #[inline]
    fn is_notified(&self) -> bool {
        match self {
            State::Notified => true,
            State::Created | State::Polling(_) | State::Waiting(_) => false,
        }
    }
}

/// An entry representing a registered listener.
struct Entry {
    /// THe state of this listener.
    state: Cell<State>,

    /// Previous entry in the linked list.
    prev: Cell<Option<NonNull<Entry>>>,

    /// Next entry in the linked list.
    next: Cell<Option<NonNull<Entry>>>,
}

impl Entry {
    /// Returns `true` if this entry has been notified.
    #[inline]
    fn is_notified(&self) -> bool {
        // Do a dummy replace operation in order to take out the state.
        let state = self.state.replace(State::Notified);

        // Put back the state.
        let is_notified = state.is_notified();
        self.state.set(state);
        is_notified
    }
}

/// A linked list of entries.
struct List {
    /// First entry in the list.
    head: Option<NonNull<Entry>>,

    /// Last entry in the list.
    tail: Option<NonNull<Entry>>,

    /// Total number of entries in the list.
    len: usize,

    /// Number of notifiable entries in the list.
    ///
    /// Notifiable entries are those that haven't been notified yet.
    notifiable: usize,
}

impl List {
    /// Inserts a new entry into the list.
    fn insert(&mut self) -> NonNull<Entry> {
        unsafe {
            // Allocate an entry that is going to become the new tail.
            let entry = NonNull::new_unchecked(Box::into_raw(Box::new(Entry {
                state: Cell::new(State::Created),
                prev: Cell::new(self.tail),
                next: Cell::new(None),
            })));

            // Replace the tail with the new entry.
            match mem::replace(&mut self.tail, Some(entry)) {
                None => self.head = Some(entry),
                Some(t) => t.as_ref().next.set(Some(entry)),
            }

            // Bump the total count and the count of notifiable entries.
            self.len += 1;
            self.notifiable += 1;

            entry
        }
    }

    /// Removes an entry from the list and returns its state.
    fn remove(&mut self, entry: NonNull<Entry>) -> State {
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

            // Deallocate and extract the state.
            let entry = Box::from_raw(entry.as_ptr());
            let state = entry.state.into_inner();

            // Update the counters.
            if !state.is_notified() {
                self.notifiable -= 1;
            }
            self.len -= 1;

            state
        }
    }

    /// Notifies an entry.
    #[cold]
    fn notify(&mut self, notify_all: bool) {
        if notify_all {
            let mut entry = self.tail;

            while let Some(e) = entry {
                let e = unsafe { e.as_ref() };
                if e.is_notified() || entry == self.head {
                    break;
                }
                entry = e.prev.get();
            }

            while let Some(e) = entry {
                let e = unsafe { e.as_ref() };
                self.set_notified(e);
                entry = e.next.get();
            }
        } else {
            if let Some(e) = self.head {
                let e = unsafe { e.as_ref() };
                self.set_notified(e);
            }
        }
    }

    /// Notifies an entry and returns `true` if it wasn't notified already.
    fn set_notified(&mut self, e: &Entry) -> bool {
        // Set the state of this entry to `Notified`.
        let state = e.state.replace(State::Notified);
        let was_notified = state.is_notified();

        // Wake the task or unpark the thread.
        match state {
            State::Notified => {}
            State::Created => {}
            State::Polling(w) => w.wake(),
            State::Waiting(t) => t.unpark(),
        }

        // Update the count of notifiable entries.
        if !was_notified {
            self.notifiable -= 1;
            true
        } else {
            false
        }
    }
}

/// Equivalent to `atomic::fence(Ordering::SeqCst)`, but in some cases faster.
#[inline]
fn full_fence() {
    if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        // HACK(stjepang): On x86 architectures there are two different ways of executing
        // a `SeqCst` fence.
        //
        // 1. `atomic::fence(SeqCst)`, which compiles into a `mfence` instruction.
        // 2. `_.compare_and_swap(_, _, SeqCst)`, which compiles into a `lock cmpxchg` instruction.
        //
        // Both instructions have the effect of a full barrier, but empirical benchmarks have shown
        // that the second one is sometimes a bit faster.
        //
        // The ideal solution here would be to use inline assembly, but we're instead creating a
        // temporary atomic variable and compare-and-exchanging its value. No sane compiler to
        // x86 platforms is going to optimize this away.
        let a = AtomicUsize::new(0);
        a.compare_and_swap(0, 1, Ordering::SeqCst);
    } else {
        atomic::fence(Ordering::SeqCst);
    }
}
