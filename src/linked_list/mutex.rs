//! Implementation of the linked list using standard library mutexes.

use crate::loom::atomic::{AtomicUsize, Ordering};
use crate::loom::cell::Cell;
use crate::notify::{GenericNotify, Internal, Notification};

use std::boxed::Box;
use std::fmt;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::Instant;
use std::usize;

/// Inner state of [`Event`].
pub(crate) struct Inner<T> {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    notified: AtomicUsize,

    /// A linked list holding registered listeners.
    list: Mutex<List<T>>,

    /// A single cached list entry to avoid allocations on the fast path of the insertion.
    // TODO: Add ability to use loom::cell::UnsafeCell
    cache: std::cell::UnsafeCell<Entry<T>>,
}

impl<T> Inner<T> {
    /// Create a new linked list.
    pub(crate) fn new() -> Self {
        Inner {
            notified: AtomicUsize::new(usize::MAX),
            list: std::sync::Mutex::new(List::<T> {
                head: None,
                tail: None,
                start: None,
                len: 0,
                notified: 0,
                cache_used: false,
            }),
            cache: std::cell::UnsafeCell::new(Entry {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(None),
            }),
        }
    }

    /// Debug output.
    #[inline]
    pub(crate) fn debug_fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = match self.list.try_lock() {
            Err(TryLockError::WouldBlock) => {
                return f
                    .debug_tuple("Event")
                    .field(&format_args!("<locked>"))
                    .finish()
            }

            Err(TryLockError::Poisoned(err)) => err.into_inner(),

            Ok(lock) => lock,
        };

        f.debug_struct("Event")
            .field("listeners_notified", &guard.notified)
            .field("listeners_total", &guard.len)
            .finish()
    }

    /// Tell whether there is enough room to notify this list.
    #[inline]
    pub(crate) fn notifyable(&self, limit: usize) -> bool {
        self.notified.load(Ordering::Acquire) < limit
    }

    /// Tell if any listeners are currently notified.
    #[inline]
    pub(crate) fn is_notified(&self) -> bool {
        self.notified.load(Ordering::Acquire) > 0
    }

    /// Get the total number of listeners.
    #[inline]
    pub(crate) fn total_listeners(&self) -> usize {
        self.lock().guard.len
    }

    /// Create a listener for this linked list.
    #[cold]
    pub(crate) fn listen(&self) -> Listener<T> {
        Listener {
            entry: Some(self.lock().insert(self.cache_ptr())),
        }
    }

    /// Notify the inner list.
    #[cold]
    pub(crate) fn notify(&self, notify: impl Notification<Tag = T>) -> usize {
        self.lock().notify(notify)
    }

    /// Discard a listener.
    #[inline]
    pub(crate) fn discard(&self, listener: &mut Listener<T>) -> bool {
        // If this listener has never picked up a notification...
        if let Some(entry) = listener.entry.take() {
            let mut list = self.lock();
            // Remove the listener from the list and return `true` if it was notified.
            if list.remove(entry, self.cache_ptr()).is_notified() {
                return true;
            }
        }
        false
    }

    /// Poll a listener.
    #[inline]
    pub(crate) fn poll(&self, listener: &mut Listener<T>, cx: &mut Context<'_>) -> Poll<T> {
        let mut list = self.lock();

        let entry = match listener.entry {
            None => unreachable!("cannot poll a completed `EventListener` future"),
            Some(entry) => entry,
        };
        let state = unsafe { &entry.as_ref().state };

        // Do a dummy replace operation in order to take out the state.
        match take_state(state) {
            State::Notified { .. } => {
                // If this listener has been notified, remove it from the list and return.
                let state = list.remove(entry, self.cache_ptr());
                drop(list);
                listener.entry = None;

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

    /// Block on a listener.
    #[inline]
    pub(crate) fn wait(&self, listener: &mut Listener<T>, deadline: Option<Instant>) -> Option<T> {
        // Take out the entry pointer and set it to `None`.
        let entry = match listener.entry.take() {
            None => unreachable!("cannot wait twice on an `EventListener`"),
            Some(entry) => entry,
        };

        // Set this listener's state to `Waiting`.
        {
            let mut list = self.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match take_state(&e.state) {
                State::Notified { .. } => {
                    // If this listener has been notified, remove it from the list and return.
                    return list.remove(entry, self.cache_ptr()).take();
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
                        return self.lock().remove(entry, self.cache_ptr()).take();
                    }

                    // Park until the deadline.
                    thread::park_timeout(deadline - now);
                }
            }

            let mut list = self.lock();
            let e = unsafe { entry.as_ref() };

            // Do a dummy replace operation in order to take out the state.
            match take_state(&e.state) {
                State::Notified { .. } => {
                    // If this listener has been notified, remove it from the list and return.
                    return list.remove(entry, self.cache_ptr()).take();
                }
                // Otherwise, set the state back to `Waiting`.
                state => e.state.set(state),
            }
        }
    }

    /// Drop a listener.
    #[inline]
    pub(crate) fn remove(&self, listener: &mut Listener<T>) {
        // If this listener has never picked up a notification...
        if let Some(entry) = listener.entry.take() {
            let mut list = self.lock();

            // But if a notification was delivered to it...
            if let State::Notified {
                additional,
                tag: Some(tag),
            } = list.remove(entry, self.cache_ptr())
            {
                let mut tag = Some(tag);

                // Then pass it on to another active listener.
                list.notify(GenericNotify::new(1, additional, || tag.take().unwrap()));
            }
        }
    }

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

/// Inner state of the `EventListener`.
pub(crate) struct Listener<T> {
    /// A pointer to this listener's entry in the linked list.
    entry: Option<NonNull<Entry<T>>>,
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
                Box::from_raw(entry.as_ptr()).state.replace(State::Created)
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
