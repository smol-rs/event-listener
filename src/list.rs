//! The inner list of listeners.

use crate::node::Node;
use crate::queue::Queue;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::cell::{Cell, UnsafeCell};
use crate::{State, Task};

use core::mem;
use core::num::NonZeroUsize;
use core::ops;

use slab::Slab;

pub(crate) struct List {
    /// The inner list.
    pub(crate) inner: Mutex<ListenerSlab>,

    /// The queue of pending operations.
    pub(crate) queue: Queue,
}

impl List {
    pub(super) fn new() -> List {
        List {
            inner: Mutex::new(ListenerSlab::new()),
            queue: Queue::new(),
        }
    }
}

/// The guard returned by [`Inner::lock`].
pub(crate) struct ListGuard<'a> {
    /// Reference to the inner state.
    pub(crate) inner: &'a crate::Inner,

    /// The locked list.
    pub(crate) guard: Option<MutexGuard<'a, ListenerSlab>>,
}

impl ListGuard<'_> {
    #[cold]
    fn process_nodes_slow(
        &mut self,
        start_node: Node,
        tasks: &mut Vec<Task>,
        guard: &mut MutexGuard<'_, ListenerSlab>,
    ) {
        // Process the start node.
        tasks.extend(start_node.apply(guard));

        // Process all remaining nodes.
        while let Some(node) = self.inner.list.queue.pop() {
            tasks.extend(node.apply(guard));
        }
    }
}

impl ops::Deref for ListGuard<'_> {
    type Target = ListenerSlab;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl ops::DerefMut for ListGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

impl Drop for ListGuard<'_> {
    fn drop(&mut self) {
        let Self { inner, guard } = self;
        let mut list = guard.take().unwrap();

        // Tasks to wakeup after releasing the lock.
        let mut tasks = vec![];

        // Process every node left in the queue.
        if let Some(start_node) = inner.list.queue.pop() {
            self.process_nodes_slow(start_node, &mut tasks, &mut list);
        }

        // Update the atomic `notified` counter.
        let notified = if list.notified < list.len() {
            list.notified
        } else {
            core::usize::MAX
        };

        self.inner.notified.store(notified, Ordering::Release);

        // Drop the actual lock.
        drop(list);

        // Wakeup all tasks.
        for task in tasks {
            task.wake();
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
pub(crate) struct ListenerSlab {
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

impl ListenerSlab {
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

/// A simple mutex type that optimistically assumes that the lock is uncontended.
pub(crate) struct Mutex<T> {
    /// The inner value.
    value: UnsafeCell<T>,

    /// Whether the mutex is locked.
    locked: AtomicBool,
}

impl<T> Mutex<T> {
    /// Create a new mutex.
    pub(crate) fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            locked: AtomicBool::new(false),
        }
    }

    /// Lock the mutex.
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        // Try to lock the mutex.
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // We have successfully locked the mutex.
            Some(MutexGuard { mutex: self })
        } else {
            self.try_lock_slow()
        }
    }

    #[cold]
    fn try_lock_slow(&self) -> Option<MutexGuard<'_, T>> {
        // Assume that the contention is short-term.
        // Spin for a while to see if the mutex becomes unlocked.
        let mut spins = 100u32;

        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // We have successfully locked the mutex.
                return Some(MutexGuard { mutex: self });
            }

            // Use atomic loads instead of compare-exchange.
            while self.locked.load(Ordering::Relaxed) {
                // Return None once we've exhausted the number of spins.
                spins = spins.checked_sub(1)?;
            }
        }
    }
}

pub(crate) struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

impl<'a, T> ops::Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<'a, T> ops::DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}
