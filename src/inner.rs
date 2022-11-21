//! The inner mechanism powering the `Event` type.

use crate::list::List;
use crate::node::Node;
use crate::queue::Queue;
use crate::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::sync::cell::UnsafeCell;
use crate::Task;

use alloc::vec;
use alloc::vec::Vec;

use core::ops;

/// Inner state of [`Event`].
pub(crate) struct Inner {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    pub(crate) notified: AtomicUsize,

    /// A linked list holding registered listeners.
    list: Mutex<List>,

    /// Queue of nodes waiting to be processed.
    queue: Queue,
}

impl Inner {
    /// Create a new `Inner`.
    pub(crate) fn new() -> Self {
        Self {
            notified: AtomicUsize::new(core::usize::MAX),
            list: Mutex::new(List::new()),
            queue: Queue::new(),
        }
    }

    /// Locks the list.
    pub(crate) fn lock(&self) -> Option<ListGuard<'_>> {
        self.list.try_lock().map(|guard| ListGuard {
            inner: self,
            guard: Some(guard),
        })
    }

    /// Push a pending operation to the queue.
    #[cold]
    pub(crate) fn push(&self, node: Node) {
        self.queue.push(node);

        // Acquire and drop the lock to make sure that the queue is flushed.
        let _guard = self.lock();
    }
}

/// The guard returned by [`Inner::lock`].
pub(crate) struct ListGuard<'a> {
    /// Reference to the inner state.
    inner: &'a Inner,

    /// The locked list.
    guard: Option<MutexGuard<'a, List>>,
}

impl ListGuard<'_> {
    #[cold]
    fn process_nodes_slow(
        &mut self,
        start_node: Node,
        tasks: &mut Vec<Task>,
        guard: &mut MutexGuard<'_, List>,
    ) {
        // Process the start node.
        tasks.extend(start_node.apply(guard));

        // Process all remaining nodes.
        while let Some(node) = self.inner.queue.pop() {
            tasks.extend(node.apply(guard));
        }
    }
}

impl ops::Deref for ListGuard<'_> {
    type Target = List;

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
        if let Some(start_node) = inner.queue.pop() {
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

/// A simple mutex type that optimistically assumes that the lock is uncontended.
struct Mutex<T> {
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

struct MutexGuard<'a, T> {
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
