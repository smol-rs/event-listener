//! The inner list of listeners.

use crate::sync::cell::Cell;
use crate::Task;

use core::mem;
use core::ptr::NonNull;

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
    /// The state of this listener.
    state: Cell<State>,

    /// Previous entry in the linked list.
    prev: Cell<Option<NonNull<Entry>>>,

    /// Next entry in the linked list.
    next: Cell<Option<NonNull<Entry>>>,
}

/// A linked list of entries.
pub(crate) struct List {
    /// First entry in the list.
    head: Option<NonNull<Entry>>,

    /// Last entry in the list.
    tail: Option<NonNull<Entry>>,

    /// The first unnotified entry in the list.
    start: Option<NonNull<Entry>>,

    /// The number of notified entries in the list.
    pub(crate) notified: usize,

    /// The number of entries in the list.
    pub(crate) len: usize,
}

impl List {
    /// Create a new, empty list.
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            start: None,
            notified: 0,
            len: 0,
        }
    }

    /// Inserts a new entry into the list.
    ///
    /// # Safety
    ///
    /// The `Entry` must be pinned in memory and able to be accessed through interior
    /// mutability, protected by the spinlock.
    pub(crate) unsafe fn insert(&mut self, entry: NonNull<Entry>) {
        // Replace the tail with the new entry.
        match mem::replace(&mut self.tail, Some(entry)) {
            None => self.head = Some(entry),
            Some(t) => {
                t.as_ref().next.set(Some(entry));
                entry.as_ref().prev.set(Some(t));
            }
        }

        // If there were no unnotified entries, this one is the first now.
        if self.start.is_none() {
            self.start = self.tail;
        }

        self.len += 1;
    }

    /// Removes an entry from the list and returns its state.
    pub(crate) unsafe fn remove(&mut self, entry: NonNull<Entry>) -> State {
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

        // Update the counters.
        if state.is_notified() {
            self.notified = self.notified.saturating_sub(1);
        }
        self.len -= 1;

        state
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
                Some(e) => unsafe {
                    // Get the entry and move the pointer forward.
                    let e = e.as_ref();
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    let was_notified = e.notify(false);

                    // Update the counter.
                    self.notified += was_notified as usize;
                },
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
                Some(e) => unsafe {
                    // Get the entry and move the pointer forward.
                    let e = e.as_ref();
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    let was_notified = e.notify(true);

                    // Update the counter.
                    self.notified += was_notified as usize;
                },
            }
        }
    }
}

impl Entry {
    /// Create a new, empty entry.
    pub(crate) fn new() -> Self {
        Self {
            state: Cell::new(State::Created),
            prev: Cell::new(None),
            next: Cell::new(None),
        }
    }

    /// Get the state of this entry.
    pub(crate) fn state(&self) -> &Cell<State> {
        &self.state
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
