//! Implementation of `event-listener` built exclusively on atomics.
//!
//! On `no_std`, we don't have access to `Mutex`, so we can't use intrusive linked lists like the `std`
//! implementation. Normally, we would use a concurrent atomic queue to store listeners, but benchmarks
//! show that using queues in this way is very slow, especially for the single threaded use-case.
//!
//! We've found that it's easier to assume that the `Event` won't be under high contention in most use
//! cases. Therefore, we use a spinlock that protects a linked list of listeners, and fall back to an
//! atomic queue if the lock is contended. Benchmarks show that this is about 20% slower than the std
//! implementation, but still much faster than using a queue.

#[path = "no_std/node.rs"]
mod node;

#[path = "no_std/queue.rs"]
mod queue;

use node::{Node, TaskWaiting};
use queue::Queue;

use crate::notify::{GenericNotify, Internal, Notification};
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::cell::{Cell, UnsafeCell};
use crate::sync::Arc;
use crate::{RegisterResult, State, Task, TaskRef};

use core::fmt;
use core::marker::PhantomData;
use core::mem;
use core::num::NonZeroUsize;
use core::ops;
use core::pin::Pin;

use alloc::vec::Vec;

impl<T> crate::Inner<T> {
    /// Locks the list.
    fn try_lock(&self) -> Option<ListGuard<'_, T>> {
        self.list.inner.try_lock().map(|guard| ListGuard {
            inner: self,
            guard: Some(guard),
        })
    }

    /// Add a new listener to the list.
    ///
    /// Does nothing if the list is already registered.
    pub(crate) fn insert(&self, mut listener: Pin<&mut Option<Listener<T>>>) {
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
        mut listener: Pin<&mut Option<Listener<T>>>,
        propogate: bool,
    ) -> Option<State<T>> {
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
                            listener: key,
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

            _ => unreachable!(),
        };

        state
    }

    /// Notifies a number of entries.
    #[cold]
    pub(crate) fn notify(&self, mut notify: impl Notification<Tag = T>) -> usize {
        match self.try_lock() {
            Some(mut guard) => {
                // Notify the listeners.
                guard.notify(notify)
            }

            None => {
                // Push it to the queue.
                let node = Node::Notify(GenericNotify::new(
                    notify.count(Internal::new()),
                    notify.is_additional(Internal::new()),
                    {
                        // Collect every tag we need.
                        let tags = {
                            let count = notify.count(Internal::new());
                            let mut tags = Vec::with_capacity(count);
                            for _ in 0..count {
                                tags.push(notify.next_tag(Internal::new()));
                            }

                            // Convert into an iterator.
                            tags.into_iter()
                        };

                        node::VecProducer(tags)
                    },
                ));

                self.list.queue.push(node);

                // We haven't notified anyone yet.
                0
            }
        }
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns `true` if the listener was already notified, and `false` otherwise. If the listener
    /// isn't inserted, returns `None`.
    pub(crate) fn register(
        &self,
        mut listener: Pin<&mut Option<Listener<T>>>,
        task: TaskRef<'_>,
    ) -> RegisterResult<T> {
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
                            return RegisterResult::Registered;
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
                            return RegisterResult::Registered;
                        }
                    }
                }

                None => return RegisterResult::NeverInserted,

                _ => unreachable!(),
            }
        }
    }
}

pub(crate) struct List<T> {
    /// The inner list.
    inner: Mutex<ListenerSlab<T>>,

    /// The queue of pending operations.
    queue: Queue<T>,
}

impl<T> List<T> {
    pub(super) fn new() -> List<T> {
        List {
            inner: Mutex::new(ListenerSlab::new()),
            queue: Queue::new(),
        }
    }
}

/// The guard returned by [`Inner::lock`].
pub(crate) struct ListGuard<'a, T> {
    /// Reference to the inner state.
    pub(crate) inner: &'a crate::Inner<T>,

    /// The locked list.
    pub(crate) guard: Option<MutexGuard<'a, ListenerSlab<T>>>,
}

impl<T> ListGuard<'_, T> {
    #[cold]
    fn process_nodes_slow(
        &mut self,
        start_node: Node<T>,
        tasks: &mut Vec<Task>,
        guard: &mut MutexGuard<'_, ListenerSlab<T>>,
    ) {
        // Process the start node.
        tasks.extend(start_node.apply(guard));

        // Process all remaining nodes.
        while let Some(node) = self.inner.list.queue.pop() {
            tasks.extend(node.apply(guard));
        }
    }
}

impl<T> ops::Deref for ListGuard<'_, T> {
    type Target = ListenerSlab<T>;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> ops::DerefMut for ListGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

impl<T> Drop for ListGuard<'_, T> {
    fn drop(&mut self) {
        let Self { inner, guard } = self;
        let mut list = guard.take().unwrap();

        // Tasks to wakeup after releasing the lock.
        let mut tasks = alloc::vec![];

        // Process every node left in the queue.
        if let Some(start_node) = inner.list.queue.pop() {
            self.process_nodes_slow(start_node, &mut tasks, &mut list);
        }

        // Update the atomic `notified` counter.
        let notified = if list.notified < list.len {
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
enum Entry<T> {
    /// Contains the listener state.
    Listener {
        /// The state of the listener.
        state: Cell<State<T>>,

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

struct TakenState<'a, T> {
    slot: &'a Cell<State<T>>,
    state: State<T>,
}

impl<T> Drop for TakenState<'_, T> {
    fn drop(&mut self) {
        self.slot
            .set(mem::replace(&mut self.state, State::NotifiedTaken));
    }
}

impl<T: fmt::Debug> fmt::Debug for TakenState<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.state, f)
    }
}

impl<T: PartialEq> PartialEq for TakenState<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.state == other.state
    }
}

impl<'a, T> TakenState<'a, T> {
    fn new(slot: &'a Cell<State<T>>) -> Self {
        let state = slot.replace(State::NotifiedTaken);
        Self { slot, state }
    }
}

impl<T: fmt::Debug> fmt::Debug for Entry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entry::Listener { state, next, prev } => f
                .debug_struct("Listener")
                .field("state", &TakenState::new(state))
                .field("prev", prev)
                .field("next", next)
                .finish(),
            Entry::Empty(next) => f.debug_tuple("Empty").field(next).finish(),
            Entry::Sentinel => f.debug_tuple("Sentinel").finish(),
        }
    }
}

impl<T: PartialEq> PartialEq for Entry<T> {
    fn eq(&self, other: &Entry<T>) -> bool {
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
                if TakenState::new(state1) != TakenState::new(state2) {
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

impl<T> Entry<T> {
    fn state(&self) -> &Cell<State<T>> {
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
pub(crate) struct ListenerSlab<T> {
    /// The raw list of entries.
    listeners: Vec<Entry<T>>,

    /// First entry in the list.
    head: Option<NonZeroUsize>,

    /// Last entry in the list.
    tail: Option<NonZeroUsize>,

    /// The first unnotified entry in the list.
    start: Option<NonZeroUsize>,

    /// The number of notified entries in the list.
    notified: usize,

    /// The total number of listeners.
    len: usize,

    /// The index of the first `Empty` entry, or the length of the list plus one if there
    /// are no empty entries.
    first_empty: NonZeroUsize,
}

impl<T> ListenerSlab<T> {
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

    /// Inserts a new entry into the list.
    pub(crate) fn insert(&mut self, state: State<T>) -> NonZeroUsize {
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
    pub(crate) fn remove(&mut self, key: NonZeroUsize, propogate: bool) -> Option<State<T>> {
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

        let mut state = match entry {
            Entry::Listener { state, .. } => state.into_inner(),
            _ => unreachable!(),
        };

        // Update the counters.
        if state.is_notified() {
            self.notified = self.notified.saturating_sub(1);

            if propogate {
                // Propogate the notification to the next entry.
                let state = mem::replace(&mut state, State::NotifiedTaken);
                if let State::Notified { tag, additional } = state {
                    let tags = {
                        let mut tag = Some(tag);

                        move || tag.take().expect("called more than once")
                    };

                    self.notify(GenericNotify::new(1, additional, tags));
                }
            }
        }
        self.len -= 1;

        Some(state)
    }

    /// Notifies a number of listeners.
    #[cold]
    pub(crate) fn notify(&mut self, mut notify: impl Notification<Tag = T>) -> usize {
        let mut n = notify.count(Internal::new());
        let is_additional = notify.is_additional(Internal::new());
        if !is_additional {
            // Make sure we're not notifying more than we have.
            if n <= self.notified {
                return 0;
            }
            n -= self.notified;
        }

        let original_count = n;
        while n > 0 {
            n -= 1;

            // Notify the next entry.
            match self.start {
                None => return original_count - n - 1,

                Some(e) => {
                    // Get the entry and move the pointer forwards.
                    let entry = &self.listeners[e.get()];
                    self.start = entry.next().get();

                    // Set the state to `Notified` and notify.
                    let tag = notify.next_tag(Internal::new());
                    if let State::Task(task) = entry.state().replace(State::Notified {
                        tag,
                        additional: is_additional,
                    }) {
                        task.wake();
                    }

                    // Bump the notified count.
                    self.notified += 1;
                }
            }
        }

        original_count - n
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns `true` if the listener was already notified, and `false` otherwise. If the listener
    /// isn't inserted, returns `None`.
    pub(crate) fn register(
        &mut self,
        mut listener: Pin<&mut Option<Listener<T>>>,
        task: TaskRef<'_>,
    ) -> RegisterResult<T> {
        let key = match *listener {
            Some(Listener::HasNode(key)) => key,
            _ => return RegisterResult::NeverInserted,
        };

        let entry = &self.listeners[key.get()];

        // Take the state out and check it.
        match entry.state().replace(State::NotifiedTaken) {
            State::Notified { tag, .. } => {
                // The listener was already notified, so we don't need to do anything.
                self.remove(key, false);
                *listener = None;
                RegisterResult::Notified(tag)
            }

            State::Task(other_task) => {
                // Only replace the task if it's not the same as the one we're registering.
                if task.will_wake(other_task.as_task_ref()) {
                    entry.state().set(State::Task(other_task));
                } else {
                    entry.state().set(State::Task(task.into_task()));
                }

                RegisterResult::Registered
            }

            _ => {
                // Register the task.
                entry.state().set(State::Task(task.into_task()));
                RegisterResult::Registered
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Listener<T> {
    /// The listener has a node inside of the linked list.
    HasNode(NonZeroUsize),

    /// The listener has an entry in the queue that may or may not have a task waiting.
    Queued(Arc<TaskWaiting>),

    /// Eat the lifetime for consistency.
    _EatLifetime(PhantomData<T>),
}

impl<T> Unpin for Listener<T> {}

impl<T> PartialEq for Listener<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::HasNode(a), Self::HasNode(b)) => a == b,
            (Self::Queued(a), Self::Queued(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Task;

    #[test]
    fn smoke_mutex() {
        let mutex = Mutex::new(0);

        {
            let mut guard = mutex.try_lock().unwrap();
            *guard += 1;
        }

        {
            let mut guard = mutex.try_lock().unwrap();
            *guard += 1;
        }

        let guard = mutex.try_lock().unwrap();
        assert_eq!(*guard, 2);
    }

    #[test]
    fn smoke_listener_slab() {
        let mut listeners = ListenerSlab::<()>::new();

        // Insert a few listeners.
        let key1 = listeners.insert(State::Created);
        let key2 = listeners.insert(State::Created);
        let key3 = listeners.insert(State::Created);

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key1));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Remove one.
        assert_eq!(listeners.remove(key2, false), Some(State::Created));

        assert_eq!(listeners.len, 2);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key1));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(2).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Empty(NonZeroUsize::new(4).unwrap())
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key1)),
                next: Cell::new(None),
            }
        );
    }

    #[test]
    fn listener_slab_notify() {
        let mut listeners = ListenerSlab::new();

        // Insert a few listeners.
        let key1 = listeners.insert(State::Created);
        let key2 = listeners.insert(State::Created);
        let key3 = listeners.insert(State::Created);

        // Notify one.
        listeners.notify(GenericNotify::new(1, true, || ()));

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 1);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key2));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: true,
                    tag: ()
                }),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Remove the notified listener.
        assert_eq!(
            listeners.remove(key1, false),
            Some(State::Notified {
                additional: true,
                tag: ()
            })
        );

        assert_eq!(listeners.len, 2);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key2));
        assert_eq!(listeners.start, Some(key2));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(1).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Empty(NonZeroUsize::new(4).unwrap())
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );
    }

    #[test]
    fn listener_slab_register() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = waker_fn::waker_fn({
            let woken = woken.clone();
            move || woken.store(true, Ordering::SeqCst)
        });

        let mut listeners = ListenerSlab::new();

        // Insert a few listeners.
        let key1 = listeners.insert(State::Created);
        let key2 = listeners.insert(State::Created);
        let key3 = listeners.insert(State::Created);

        // Register one.
        assert_eq!(
            listeners.register(
                Pin::new(&mut Some(Listener::HasNode(key2))),
                TaskRef::Waker(&waker)
            ),
            RegisterResult::Registered
        );

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key1));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Task(Task::Waker(waker.clone()))),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Notify the listener.
        listeners.notify(GenericNotify::new(2, false, || ()));

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 2);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key3));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        assert!(woken.load(Ordering::SeqCst));
        assert_eq!(
            listeners.register(
                Pin::new(&mut Some(Listener::HasNode(key2))),
                TaskRef::Waker(&waker)
            ),
            RegisterResult::Notified(())
        );
    }

    #[test]
    fn listener_slab_notify_prop() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = waker_fn::waker_fn({
            let woken = woken.clone();
            move || woken.store(true, Ordering::SeqCst)
        });

        let mut listeners = ListenerSlab::new();

        // Insert a few listeners.
        let key1 = listeners.insert(State::Created);
        let key2 = listeners.insert(State::Created);
        let key3 = listeners.insert(State::Created);

        // Register one.
        assert_eq!(
            listeners.register(
                Pin::new(&mut Some(Listener::HasNode(key2))),
                TaskRef::Waker(&waker)
            ),
            RegisterResult::Registered
        );

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key1));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Task(Task::Waker(waker.clone()))),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Notify the first listener.
        listeners.notify(GenericNotify::new(1, false, || ()));

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 1);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key2));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Task(Task::Waker(waker.clone()))),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Calling notify again should not change anything.
        listeners.notify(GenericNotify::new(1, false, || ()));

        assert_eq!(listeners.len, 3);
        assert_eq!(listeners.notified, 1);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key1));
        assert_eq!(listeners.start, Some(key2));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(4).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(None),
                next: Cell::new(Some(key2)),
            }
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Task(Task::Waker(waker.clone()))),
                prev: Cell::new(Some(key1)),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Remove the first listener.
        assert_eq!(
            listeners.remove(key1, false),
            Some(State::Notified {
                additional: false,
                tag: ()
            })
        );

        assert_eq!(listeners.len, 2);
        assert_eq!(listeners.notified, 0);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key2));
        assert_eq!(listeners.start, Some(key2));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(1).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Empty(NonZeroUsize::new(4).unwrap())
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Task(Task::Waker(waker))),
                prev: Cell::new(None),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Notify the second listener.
        listeners.notify(GenericNotify::new(1, false, || ()));
        assert!(woken.load(Ordering::SeqCst));

        assert_eq!(listeners.len, 2);
        assert_eq!(listeners.notified, 1);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key2));
        assert_eq!(listeners.start, Some(key3));
        assert_eq!(listeners.first_empty, NonZeroUsize::new(1).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Empty(NonZeroUsize::new(4).unwrap())
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(None),
                next: Cell::new(Some(key3)),
            }
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Created),
                prev: Cell::new(Some(key2)),
                next: Cell::new(None),
            }
        );

        // Remove and propogate the second listener.
        assert_eq!(listeners.remove(key2, true), Some(State::NotifiedTaken));

        // The third listener should be notified.
        assert_eq!(listeners.len, 1);
        assert_eq!(listeners.notified, 1);
        assert_eq!(listeners.tail, Some(key3));
        assert_eq!(listeners.head, Some(key3));
        assert_eq!(listeners.start, None);
        assert_eq!(listeners.first_empty, NonZeroUsize::new(2).unwrap());
        assert_eq!(listeners.listeners[0], Entry::Sentinel);
        assert_eq!(
            listeners.listeners[1],
            Entry::Empty(NonZeroUsize::new(4).unwrap())
        );
        assert_eq!(
            listeners.listeners[2],
            Entry::Empty(NonZeroUsize::new(1).unwrap())
        );
        assert_eq!(
            listeners.listeners[3],
            Entry::Listener {
                state: Cell::new(State::Notified {
                    additional: false,
                    tag: (),
                }),
                prev: Cell::new(None),
                next: Cell::new(None),
            }
        );

        // Remove the third listener.
        assert_eq!(
            listeners.remove(key3, false),
            Some(State::Notified {
                additional: false,
                tag: ()
            })
        );
    }

    #[test]
    fn uncontended_inner() {
        let inner = crate::Inner::new();

        // Register two listeners.
        let (mut listener1, mut listener2, mut listener3) = (None, None, None);
        inner.insert(Pin::new(&mut listener1));
        inner.insert(Pin::new(&mut listener2));
        inner.insert(Pin::new(&mut listener3));

        assert_eq!(
            listener1,
            Some(Listener::HasNode(NonZeroUsize::new(1).unwrap()))
        );
        assert_eq!(
            listener2,
            Some(Listener::HasNode(NonZeroUsize::new(2).unwrap()))
        );

        // Register a waker in the second listener.
        let woken = Arc::new(AtomicBool::new(false));
        let waker = waker_fn::waker_fn({
            let woken = woken.clone();
            move || woken.store(true, Ordering::SeqCst)
        });
        assert_eq!(
            inner.register(Pin::new(&mut listener2), TaskRef::Waker(&waker)),
            RegisterResult::Registered
        );

        // Notify the first listener.
        inner.notify(GenericNotify::new(1, false, || ()));
        assert!(!woken.load(Ordering::SeqCst));

        // Another notify should do nothing.
        inner.notify(GenericNotify::new(1, false, || ()));
        assert!(!woken.load(Ordering::SeqCst));

        // Receive the notification.
        assert_eq!(
            inner.register(Pin::new(&mut listener1), TaskRef::Waker(&waker)),
            RegisterResult::Notified(())
        );

        // First listener is already removed.
        assert!(listener1.is_none());

        // Notify the second listener.
        inner.notify(GenericNotify::new(1, false, || ()));
        assert!(woken.load(Ordering::SeqCst));

        // Remove the second listener and propogate the notification.
        assert_eq!(
            inner.remove(Pin::new(&mut listener2), true),
            Some(State::NotifiedTaken)
        );

        // Second listener is already removed.
        assert!(listener2.is_none());

        // Third listener should be notified.
        assert_eq!(
            inner.register(Pin::new(&mut listener3), TaskRef::Waker(&waker)),
            RegisterResult::Notified(())
        );
    }
}
