//! The inner list of listeners.

use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::cell::Cell;
use crate::Task;

use alloc::boxed::Box;

use core::mem;
use core::ptr::{self, NonNull};

/// The state of a listener.
pub(crate) enum State {
    /// It has just been created.
    Created,

    /// It has received a notification.
    ///
    /// The `bool` is `true` if this was an "additional" notification.
    Notified(bool),

    /// A task is polling it.
    Task(Task),
}

impl State {
    /// Returns `true` if this is the `Notified` state.
    #[inline]
    pub(crate) fn is_notified(&self) -> bool {
        match self {
            State::Notified(_) => true,
            _ => false,
        }
    }
}

/// An entry representing a registered listener.
pub(crate) struct Entry {
    /// Shared state used to coordinate the listener under contention.
    ///
    /// This is the only field that can be accessed without the list being locked.
    shared_state: SharedState,

    /// The state of this listener.
    state: Cell<State>,

    /// Previous entry in the linked list.
    prev: Cell<Option<NonNull<Entry>>>,

    /// Next entry in the linked list.
    next: Cell<Option<NonNull<Entry>>>,
}

struct SharedState {
    /// Information about this shared state.
    state: AtomicUsize,

    /// A task to wake up once we are inserted into the list.
    insert_task: Cell<Option<Task>>,
}

/// A linked list of entries.
pub(crate) struct List {
    /// First entry in the list.
    head: Option<NonNull<Entry>>,

    /// Last entry in the list.
    tail: Option<NonNull<Entry>>,

    /// The first unnotified entry in the list.
    start: Option<NonNull<Entry>>,

    /// Total number of entries in the list.
    pub(crate) len: usize,

    /// The number of notified entries in the list.
    pub(crate) notified: usize,

    /// Whether the cached entry is used.
    cache_used: bool,
}

impl List {
    /// Create a new, empty list.
    pub(crate) fn new() -> Self {
        Self {
            head: None,
            tail: None,
            start: None,
            len: 0,
            notified: 0,
            cache_used: false,
        }
    }

    /// Allocate a new entry.
    pub(crate) unsafe fn alloc(&mut self, cache: NonNull<Entry>) -> NonNull<Entry> {
        if self.cache_used {
            // Allocate an entry that is going to become the new tail.
            NonNull::new_unchecked(Box::into_raw(Box::new(Entry::new())))
        } else {
            // No need to allocate - we can use the cached entry.
            self.cache_used = true;
            cache.as_ptr().write(Entry::new());
            cache
        }
    }

    /// Inserts a new entry into the list.
    pub(crate) fn insert(&mut self, entry: NonNull<Entry>) {
        // Replace the tail with the new entry.
        match mem::replace(&mut self.tail, Some(entry)) {
            None => self.head = Some(entry),
            Some(t) => unsafe {
                t.as_ref().next.set(Some(entry));
                entry.as_ref().prev.set(Some(t));
            },
        }

        // If there were no unnotified entries, this one is the first now.
        if self.start.is_none() {
            self.start = self.tail;
        }

        // Bump the entry count.
        self.len += 1;
    }

    /// De-allocate an entry.
    unsafe fn dealloc(&mut self, entry: NonNull<Entry>, cache: NonNull<Entry>) -> State {
        if ptr::eq(entry.as_ptr(), cache.as_ptr()) {
            // Free the cached entry.
            self.cache_used = false;
            entry.as_ref().state.replace(State::Created)
        } else {
            // Deallocate the entry.
            #[cfg(not(all(feature = "loom", loom)))]
            {
                Box::from_raw(entry.as_ptr()).state.into_inner()
            }

            // Loom doesn't support Cell::into_inner() for some reason.
            #[cfg(all(feature = "loom", loom))]
            {
                Box::from_raw(entry.as_ptr()).state.replace(State::Created)
            }
        }
    }

    /// Removes an entry from the list and returns its state.
    pub(crate) fn remove(&mut self, entry: NonNull<Entry>, cache: NonNull<Entry>) -> State {
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
            let state = entry.as_ref().state.replace(State::Created);

            // Delete the entry.
            self.dealloc(entry, cache);

            // Update the counters.
            if state.is_notified() {
                self.notified = self.notified.saturating_sub(1);
            }
            self.len = self.len.saturating_sub(1);

            state
        }
    }

    /// Notifies a number of entries, either normally or as an additional notification.
    #[cold]
    pub(crate) fn notify(&mut self, count: usize, additional: bool) {
        if additional {
            self.notify_additional(count);
        } else {
            self.notify_unnotified(count);
        }
    }

    /// Notifies a number of entries.
    #[cold]
    pub(crate) fn notify_unnotified(&mut self, mut n: usize) {
        if n <= self.notified {
            return;
        }
        n -= self.notified;

        while n > 0 {
            n -= 1;

            // Notify the first unnotified entry.
            match self.start {
                None => break,
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = unsafe { e.as_ref() };
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    let was_notified = e.notify(false);

                    // Update the counter.
                    self.notified += was_notified as usize;
                }
            }
        }
    }

    /// Notifies a number of additional entries.
    #[cold]
    pub(crate) fn notify_additional(&mut self, mut n: usize) {
        while n > 0 {
            n -= 1;

            // Notify the first unnotified entry.
            match self.start {
                None => break,
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = unsafe { e.as_ref() };
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    let was_notified = e.notify(true);

                    // Update the counter.
                    self.notified += was_notified as usize;
                }
            }
        }
    }
}

impl Entry {
    /// Create a new, empty entry.
    pub(crate) fn new() -> Self {
        Self {
            shared_state: SharedState {
                state: AtomicUsize::new(0),
                insert_task: Cell::new(None),
            },
            state: Cell::new(State::Created),
            prev: Cell::new(None),
            next: Cell::new(None),
        }
    }

    /// Get the state of this entry.
    pub(crate) fn state(&self) -> &Cell<State> {
        &self.state
    }

    /// Tell whether this entry is currently queued.
    ///
    /// This is only ever used as an optimization for `wait_internal`, hence that fact that
    /// it is `std`-exclusive
    #[cfg(feature = "std")]
    pub(crate) fn is_queued(&self) -> bool {
        self.shared_state.state.load(Ordering::Acquire) & QUEUED != 0
    }

    /// Write to the temporary task.
    #[cold]
    #[cfg(feature = "std")]
    pub(crate) fn write_task(&self, task: Task) {
        // Acquire the WRITING_STATE lock.
        let mut state = self
            .shared_state
            .state
            .fetch_or(WRITING_STATE, Ordering::AcqRel);

        // Wait until the WRITING_STATE lock is released.
        while state & WRITING_STATE != 0 {
            state = self
                .shared_state
                .state
                .fetch_or(WRITING_STATE, Ordering::AcqRel);
            crate::yield_now();
        }

        // Write the task.
        self.shared_state.insert_task.set(Some(task));

        // Release the WRITING_STATE lock.
        self.shared_state
            .state
            .fetch_and(!WRITING_STATE, Ordering::Release);
    }

    /// Dequeue the entry.
    pub(crate) fn dequeue(&self) -> Option<Task> {
        // Acquire the WRITING_STATE lock.
        let mut state = self
            .shared_state
            .state
            .fetch_or(WRITING_STATE, Ordering::AcqRel);

        // Wait until the WRITING_STATE lock is released.
        while state & WRITING_STATE != 0 {
            state = self
                .shared_state
                .state
                .fetch_or(WRITING_STATE, Ordering::AcqRel);
            crate::yield_now();
        }

        // Read the task.
        let task = self.shared_state.insert_task.take();

        // Release the WRITING_STATE lock and also remove the QUEUED bit.
        self.shared_state
            .state
            .fetch_and(!WRITING_STATE & !QUEUED, Ordering::Release);

        task
    }

    /// Indicate that this entry has been queued.
    pub(crate) fn enqueue(&self) {
        self.shared_state.state.fetch_or(QUEUED, Ordering::SeqCst);
    }

    /// Indicate that this entry has been notified.
    #[cold]
    pub(crate) fn notify(&self, additional: bool) -> bool {
        match self.state.replace(State::Notified(additional)) {
            State::Notified(_) => {}
            State::Created => {}
            State::Task(w) => w.wake(),
        }

        // Return whether the notification would have had any effect.
        true
    }
}

/// Set if we are currently queued.
const QUEUED: usize = 1 << 0;

/// Whether or not we are currently writing to the `insert_task` variable, synchronously.
const WRITING_STATE: usize = 1 << 1;
