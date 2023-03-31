//! The inner list of listeners.

#[path = "no_std/node.rs"]
mod node;

#[path = "no_std/queue.rs"]
mod queue;

use node::{Node, TaskWaiting};
use queue::Queue;

use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::cell::{Cell, UnsafeCell};
use crate::sync::Arc;
use crate::{State, Task, TaskRef};

use core::fmt;
use core::mem;
use core::num::NonZeroUsize;
use core::ops;
use core::pin::Pin;

use alloc::vec::Vec;

impl crate::Inner {
    /// Locks the list.
    fn try_lock(&self) -> Option<ListGuard<'_>> {
        self.list.inner.try_lock().map(|guard| ListGuard {
            inner: self,
            guard: Some(guard),
        })
    }

    /// Add a new listener to the list.
    ///
    /// Does nothing if the list is already registered.
    pub(crate) fn insert(&self, mut listener: Pin<&mut Option<Listener>>) {
        if listener.as_ref().as_pin_ref().is_some() {
            // Already inserted.
            return;
        }

        match self.try_lock() {
            Some(mut lock) => {
                let key = lock.insert(State::Created);
                *listener = Some(Listener::HasNode(key));
            }

            None => {
                // Push it to the queue.
                let (node, task_waiting) = Node::listener();
                self.list.queue.push(node);
                *listener = Some(Listener::Queued(task_waiting));
            }
        }
    }

    /// Remove a listener from the list.
    pub(crate) fn remove(
        &self,
        mut listener: Pin<&mut Option<Listener>>,
        propogate: bool,
    ) -> Option<State> {
        let state = match listener.as_mut().take() {
            Some(Listener::HasNode(key)) => {
                match self.try_lock() {
                    Some(mut list) => {
                        // Fast path removal.
                        list.remove(key, propogate)
                    }

                    None => {
                        // Slow path removal.
                        // This is why intrusive lists don't work on no_std.
                        let node = Node::RemoveListener {
                            key,
                            propagate: propogate,
                        };

                        self.list.queue.push(node);

                        None
                    }
                }
            }

            Some(Listener::Queued(_)) => {
                // This won't be added after we drop the lock.
                None
            }

            None => None,
        };

        state
    }

    /// Notifies a number of entries.
    #[cold]
    pub(crate) fn notify(&self, n: usize, additional: bool) {
        match self.try_lock() {
            Some(mut guard) => {
                // Notify the listeners.
                guard.notify(n, additional);
            }

            None => {
                // Push it to the queue.
                let node = Node::Notify {
                    count: n,
                    additional,
                };

                self.list.queue.push(node);
            }
        }
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns `true` if the listener was already notified, and `false` otherwise. If the listener
    /// isn't inserted, returns `None`.
    pub(crate) fn register(
        &self,
        mut listener: Pin<&mut Option<Listener>>,
        task: TaskRef<'_>,
    ) -> Option<bool> {
        loop {
            match listener.as_mut().take() {
                Some(Listener::HasNode(key)) => {
                    *listener = Some(Listener::HasNode(key));
                    match self.try_lock() {
                        Some(mut guard) => {
                            // Fast path registration.
                            return guard.register(listener, task);
                        }

                        None => {
                            // Wait for the lock.
                            let node = Node::Waiting(task.into_task());
                            self.list.queue.push(node);
                            return Some(false);
                        }
                    }
                }

                Some(Listener::Queued(task_waiting)) => {
                    // Are we done yet?
                    match task_waiting.status() {
                        Some(key) => {
                            // We're inserted now, adjust state.
                            *listener = Some(Listener::HasNode(key));
                        }

                        None => {
                            // We're still queued, so register the task.
                            task_waiting.register(task.into_task());
                            *listener = Some(Listener::Queued(task_waiting));
                            return None;
                        }
                    }
                }

                _ => return None,
            }
        }
    }
}

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
enum Entry {
    /// Contains the listener state.
    Listener {
        /// The state of the listener.
        state: Cell<State>,

        /// The previous listener in the list.
        prev: Cell<Option<NonZeroUsize>>,

        /// The next listener in the list.
        next: Cell<Option<NonZeroUsize>>,
    },

    /// An empty slot that contains the index of the next empty slot.
    Empty(NonZeroUsize),

    /// Sentinel value.
    Sentinel,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct TakenState<'a> {
            state: Option<State>,
            cell: &'a Cell<State>,
        }

        impl Drop for TakenState<'_> {
            fn drop(&mut self) {
                self.cell.set(self.state.take().unwrap());
            }
        }

        impl fmt::Debug for TakenState<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(self.state.as_ref().unwrap(), f)
            }
        }

        match self {
            Entry::Listener { state, next, prev } => {
                let taken = TakenState {
                    state: Some(state.replace(State::Created)),
                    cell: state,
                };

                f.debug_struct("Listener")
                    .field("state", &taken)
                    .field("prev", prev)
                    .field("next", next)
                    .finish()
            }
            Entry::Empty(next) => f.debug_tuple("Empty").field(next).finish(),
            Entry::Sentinel => f.debug_tuple("Sentinel").finish(),
        }
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Entry) -> bool {
        struct RestoreState<'a> {
            state: Option<State>,
            cell: &'a Cell<State>,
        }

        impl Drop for RestoreState<'_> {
            fn drop(&mut self) {
                self.cell.set(self.state.take().unwrap());
            }
        }

        match (self, other) {
            (
                Self::Listener {
                    state: state1,
                    prev: prev1,
                    next: next1,
                },
                Self::Listener {
                    state: state2,
                    prev: prev2,
                    next: next2,
                },
            ) => {
                let taken1 = RestoreState {
                    state: Some(state1.replace(State::Created)),
                    cell: state1,
                };

                let taken2 = RestoreState {
                    state: Some(state2.replace(State::Created)),
                    cell: state2,
                };

                if taken1.state != taken2.state {
                    return false;
                }

                prev1.get() == prev2.get() && next1.get() == next2.get()
            }
            (Self::Empty(next1), Self::Empty(next2)) => next1 == next2,
            (Self::Sentinel, Self::Sentinel) => true,
            _ => false,
        }
    }
}

impl Entry {
    fn state(&self) -> &Cell<State> {
        match self {
            Entry::Listener { state, .. } => state,
            _ => unreachable!(),
        }
    }

    fn prev(&self) -> &Cell<Option<NonZeroUsize>> {
        match self {
            Entry::Listener { prev, .. } => prev,
            _ => unreachable!(),
        }
    }

    fn next(&self) -> &Cell<Option<NonZeroUsize>> {
        match self {
            Entry::Listener { next, .. } => next,
            _ => unreachable!(),
        }
    }
}

/// A linked list of entries.
pub(crate) struct ListenerSlab {
    /// The raw list of entries.
    listeners: Vec<Entry>,

    /// First entry in the list.
    head: Option<NonZeroUsize>,

    /// Last entry in the list.
    tail: Option<NonZeroUsize>,

    /// The first unnotified entry in the list.
    start: Option<NonZeroUsize>,

    /// The number of notified entries in the list.
    pub(crate) notified: usize,

    /// The total number of listeners.
    len: usize,

    /// The index of the first `Empty` entry, or the length of the list plus one if there
    /// are no empty entries.
    first_empty: NonZeroUsize,
}

impl ListenerSlab {
    /// Create a new, empty list.
    pub(crate) fn new() -> Self {
        Self {
            listeners: alloc::vec![Entry::Sentinel],
            head: None,
            tail: None,
            start: None,
            notified: 0,
            len: 0,
            first_empty: unsafe { NonZeroUsize::new_unchecked(1) },
        }
    }

    /// Get the number of entries in the list.
    pub(crate) fn len(&self) -> usize {
        self.listeners.len() - 1
    }

    /// Inserts a new entry into the list.
    pub(crate) fn insert(&mut self, state: State) -> NonZeroUsize {
        // Add the new entry into the list.
        let key = {
            let entry = Entry::Listener {
                state: Cell::new(state),
                prev: Cell::new(self.tail),
                next: Cell::new(None),
            };

            let key = self.first_empty;
            if self.first_empty.get() == self.listeners.len() {
                // No empty entries, so add a new entry.
                self.listeners.push(entry);

                // SAFETY: Guaranteed to not overflow, since the Vec would have panicked already.
                self.first_empty = unsafe { NonZeroUsize::new_unchecked(self.listeners.len()) };
            } else {
                // There is an empty entry, so replace it.
                let slot = &mut self.listeners[key.get()];
                let next = match mem::replace(slot, entry) {
                    Entry::Empty(next) => next,
                    _ => unreachable!(),
                };

                self.first_empty = next;
            }

            key
        };

        // Replace the tail with the new entry.
        match mem::replace(&mut self.tail, Some(key)) {
            None => self.head = Some(key),
            Some(tail) => {
                let tail = &self.listeners[tail.get()];
                tail.next().set(Some(key));
            }
        }

        // If there are no listeners that have been notified, then the new listener is the next
        // listener to be notified.
        if self.start.is_none() {
            self.start = Some(key);
        }

        // Increment the length.
        self.len += 1;

        key
    }

    /// Removes an entry from the list and returns its state.
    pub(crate) fn remove(&mut self, key: NonZeroUsize, propogate: bool) -> Option<State> {
        let entry = &self.listeners[key.get()];
        let prev = entry.prev().get();
        let next = entry.next().get();

        // Unlink from the previous entry.
        match prev {
            None => self.head = next,
            Some(p) => self.listeners[p.get()].next().set(next),
        }

        // Unlink from the next entry.
        match next {
            None => self.tail = prev,
            Some(n) => self.listeners[n.get()].prev().set(prev),
        }

        // If this was the first unnotified entry, move the pointer to the next one.
        if self.start == Some(key) {
            self.start = next;
        }

        // Extract the state.
        let entry = mem::replace(
            &mut self.listeners[key.get()],
            Entry::Empty(self.first_empty),
        );
        self.first_empty = key;

        let state = match entry {
            Entry::Listener { state, .. } => state.into_inner(),
            _ => unreachable!(),
        };

        // Update the counters.
        if state.is_notified() {
            self.notified = self.notified.saturating_sub(1);

            if propogate {
                // Propogate the notification to the next entry.
                if let State::Notified(additional) = state {
                    self.notify(1, additional);
                }
            }
        }

        Some(state)
    }

    /// Notifies a number of listeners.
    #[cold]
    pub(crate) fn notify(&mut self, mut n: usize, additional: bool) {
        if !additional {
            // Make sure we're not notifying more than we have.
            if n <= self.notified {
                return;
            }
            n -= self.notified;
        }

        while n > 0 {
            n -= 1;

            // Notify the next entry.
            match self.start {
                None => break,

                Some(e) => {
                    // Get the entry and move the pointer forwards.
                    let entry = &self.listeners[e.get()];
                    self.start = entry.next().get();

                    // Set the state to `Notified` and notify.
                    if let State::Task(task) = entry.state().replace(State::Notified(additional)) {
                        task.wake();
                    }

                    // Bump the notified count.
                    self.notified += 1;
                }
            }
        }
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns `true` if the listener was already notified, and `false` otherwise. If the listener
    /// isn't inserted, returns `None`.
    pub(crate) fn register(
        &mut self,
        mut listener: Pin<&mut Option<Listener>>,
        task: TaskRef<'_>,
    ) -> Option<bool> {
        let key = match *listener {
            Some(Listener::HasNode(key)) => key,
            _ => return None,
        };

        let entry = &self.listeners[key.get()];

        // Take the state out and check it.
        match entry.state().replace(State::NotifiedTaken) {
            State::Notified(_) | State::NotifiedTaken => {
                // The listener was already notified, so we don't need to do anything.
                self.remove(key, false)?;
                *listener = None;
                Some(true)
            }

            State::Task(other_task) => {
                // Only replace the task if it's not the same as the one we're registering.
                if task.will_wake(other_task.as_task_ref()) {
                    entry.state().set(State::Task(other_task));
                } else {
                    entry.state().set(State::Task(task.into_task()));
                }

                Some(false)
            }

            _ => {
                // Register the task.
                entry.state().set(State::Task(task.into_task()));
                Some(false)
            }
        }
    }
}

pub(crate) enum Listener {
    /// The listener has a node inside of the linked list.
    HasNode(NonZeroUsize),

    /// The listener has an entry in the queue that may or may not have a task waiting.
    Queued(Arc<TaskWaiting>),
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
