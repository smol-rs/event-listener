//! The inner list of listeners.

use crate::sync::cell::Cell;
use crate::Task;

use core::mem;
use core::num::NonZeroUsize;

use slab::Slab;

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
    prev: Cell<Option<NonZeroUsize>>,

    /// Next entry in the linked list.
    next: Cell<Option<NonZeroUsize>>,
}

/// A linked list of entries.
pub(crate) struct List {
    /// The raw list of entries.
    entries: Slab<Entry>,

    /// First entry in the list.
    head: Option<NonZeroUsize>,

    /// Last entry in the list.
    tail: Option<NonZeroUsize>,

    /// The first unnotified entry in the list.
    start: Option<NonZeroUsize>,

    /// The number of notified entries in the list.
    pub(crate) notified: usize,
}

impl List {
    /// Create a new, empty list.
    pub(crate) fn new() -> Self {
        // Create a Slab with a permanent entry occupying index 0, so that
        // it is never used (and we can therefore use 0 as a sentinel value).
        let mut entries = Slab::new();
        entries.insert(Entry::new());

        Self {
            entries,
            head: None,
            tail: None,
            start: None,
            notified: 0,
        }
    }

    /// Get the number of entries in the list.
    pub(crate) fn len(&self) -> usize {
        self.entries.len() - 1
    }

    /// Get the state of the entry at the given index.
    pub(crate) fn state(&self, index: NonZeroUsize) -> &Cell<State> {
        &self.entries[index.get()].state
    }

    /// Inserts a new entry into the list.
    pub(crate) fn insert(&mut self, entry: Entry) -> NonZeroUsize {
        // Replace the tail with the new entry.
        let key = NonZeroUsize::new(self.entries.vacant_key()).unwrap();
        match mem::replace(&mut self.tail, Some(key)) {
            None => self.head = Some(key),
            Some(t) => {
                self.entries[t.get()].next.set(Some(key));
                entry.prev.set(Some(t));
            }
        }

        // If there were no unnotified entries, this one is the first now.
        if self.start.is_none() {
            self.start = self.tail;
        }

        // Insert the entry into the slab.
        self.entries.insert(entry);

        // Return the key.
        key
    }

    /// Removes an entry from the list and returns its state.
    pub(crate) fn remove(&mut self, key: NonZeroUsize) -> State {
        let entry = self.entries.remove(key.get());
        let prev = entry.prev.get();
        let next = entry.next.get();

        // Unlink from the previous entry.
        match prev {
            None => self.head = next,
            Some(p) => self.entries[p.get()].next.set(next),
        }

        // Unlink from the next entry.
        match next {
            None => self.tail = prev,
            Some(n) => self.entries[n.get()].prev.set(prev),
        }

        // If this was the first unnotified entry, move the pointer to the next one.
        if self.start == Some(key) {
            self.start = next;
        }

        // Extract the state.
        let state = entry.state.replace(State::Created);

        // Update the counters.
        if state.is_notified() {
            self.notified = self.notified.saturating_sub(1);
        }

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
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = &self.entries[e.get()];
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
                    let e = &self.entries[e.get()];
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
            state: Cell::new(State::Created),
            prev: Cell::new(None),
            next: Cell::new(None),
        }
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
