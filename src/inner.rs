//! The inner mechanism powering the `Event` type.

use crate::list::List;
use crate::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::sync::cell::UnsafeCell;

use core::ops;

/// Inner state of [`Event`].
pub(crate) struct Inner {
    /// The number of notified entries, or `usize::MAX` if all of them have been notified.
    ///
    /// If there are no entries, this value is set to `usize::MAX`.
    pub(crate) notified: AtomicUsize,

    /// A linked list holding registered listeners.
    list: Mutex<List>,
}

impl Inner {
    /// Create a new `Inner`.
    pub(crate) const fn new() -> Self {
        Self {
            notified: AtomicUsize::new(core::usize::MAX),
            list: Mutex::new(List::new()),
        }
    }

    /// Locks the list.
    pub(crate) fn lock(&self) -> ListGuard<'_> {
        let guard = self.list.lock();

        ListGuard { inner: self, guard }
    }
}

/// The guard returned by [`Inner::lock`].
pub(crate) struct ListGuard<'a> {
    /// Reference to the inner state.
    inner: &'a Inner,

    /// The locked list.
    guard: MutexGuard<'a, List>,
}

impl ops::Deref for ListGuard<'_> {
    type Target = List;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl ops::DerefMut for ListGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for ListGuard<'_> {
    fn drop(&mut self) {
        let Self { inner, guard: list } = self;

        // Update the atomic `notified` counter.
        let notified = if list.notified < list.len {
            list.notified
        } else {
            core::usize::MAX
        };

        inner.notified.store(notified, Ordering::Release);
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
    pub(crate) const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            locked: AtomicBool::new(false),
        }
    }

    /// Lock the mutex.
    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        // Try to lock the mutex.
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // We have successfully locked the mutex.
            MutexGuard { mutex: self }
        } else {
            self.lock_slow()
        }
    }

    #[cold]
    fn lock_slow(&self) -> MutexGuard<'_, T> {
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // We have successfully locked the mutex.
                return MutexGuard { mutex: self };
            }

            // Use atomic loads instead of compare-exchange.
            while self.locked.load(Ordering::Relaxed) {
                #[allow(deprecated)]
                crate::sync::atomic::spin_loop_hint();
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
