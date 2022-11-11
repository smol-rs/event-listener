use crate::sync::atomic::{AtomicPtr, Ordering};
use crate::sync::Arc;
use core::mem::ManuallyDrop;
use core::ptr;

/// An `Arc` that can be atomically racily initialized.
pub(crate) struct RacyArc<T> {
    /// The inner `Arc`.
    ///
    /// This is `null` if the `RacyArc` has not been initialized yet.
    ptr: AtomicPtr<T>,
}

impl<T> RacyArc<T> {
    /// Create a new, empty `RacyArc`.
    pub(crate) const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Try to get a reference to the inner `T`.
    pub(crate) fn get(&self) -> Option<&T> {
        let ptr = self.ptr.load(Ordering::Acquire);
        unsafe { ptr.as_ref() }
    }

    /// Initialize the `RacyArc` with the given `T`, and returning an
    /// `Arc` to it.
    pub(crate) fn with_or_init<R>(
        &self,
        init: impl FnOnce() -> T,
        with: impl FnOnce(&Arc<T>) -> R,
    ) -> R {
        // Load the current value.
        let mut inner = self.ptr.load(Ordering::Acquire);

        // Initialize the state if this is its first use.
        if inner.is_null() {
            // Allocate on the heap.
            let new = Arc::new(init());
            // Convert the heap-allocated state into a raw pointer.
            let new = Arc::into_raw(new) as *mut T;

            // Attempt to replace the null-pointer with the new state pointer.
            inner = self
                .ptr
                .compare_exchange(inner, new, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|x| x);

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

        // Convert the raw pointer back into an `Arc`.
        let arc = ManuallyDrop::new(unsafe { Arc::from_raw(inner) });
        with(&arc)
    }
}

impl<T> Drop for RacyArc<T> {
    fn drop(&mut self) {
        // Load the current value.
        let inner = *self.ptr.get_mut();

        // If the value is not null, then drop it.
        if !inner.is_null() {
            unsafe {
                drop(Arc::from_raw(inner));
            }
        }
    }
}
