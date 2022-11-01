//! Various utility structures.

use crate::sync::atomic::{AtomicPtr, Ordering};
use crate::sync::{Arc, AtomicWithMut};

use core::mem::ManuallyDrop;
use core::ptr;

/// A wrapper around an `Arc` that is initialized atomically in a racy manner.
#[derive(Default)]
pub(crate) struct RacyArc<T> {
    ptr: AtomicPtr<T>,
}

unsafe impl<T: Send + Sync> Send for RacyArc<T> {}
unsafe impl<T: Send + Sync> Sync for RacyArc<T> {}

impl<T> RacyArc<T> {
    /// Create a new, empty `RacyArc`.
    pub(crate) const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Try to load the inner `T` or return `None` if it is not initialized.
    pub(crate) fn get(&self) -> Option<&T> {
        let ptr = self.ptr.load(Ordering::Acquire);

        // SAFETY: `ptr` is either a valid `T` reference or `null_mut()`.
        unsafe { ptr.as_ref() }
    }

    /// Load the inner `T`, initializing it using the given closure if it is not
    /// initialized.
    pub(crate) fn get_or_init(&self, init: impl FnOnce() -> Arc<T>) -> ManuallyDrop<Arc<T>> {
        let mut ptr = self.ptr.load(Ordering::Acquire);

        if ptr.is_null() {
            // Initialize the `Arc` using the given closure.
            let data = init();

            // Convert it to a pointer.
            let new = Arc::into_raw(data) as *mut T;

            // Try to swap the pointer.
            ptr = self
                .ptr
                .compare_exchange(ptr, new, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|x| x);

            // Check if the pointer we tried to replace is null.
            if ptr.is_null() {
                // If it is, we successfully initialized the `RacyArc`.
                ptr = new;
            } else {
                // If it is not, we failed to initialize the `RacyArc` and we
                // need to drop the `Arc` we created.
                drop(unsafe { Arc::from_raw(new) });
            }
        }

        // SAFETY: `ptr` is now a valid T reference.
        ManuallyDrop::new(unsafe { Arc::from_raw(ptr) })
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        self.ptr.load(Ordering::Acquire)
    }
}

impl<T> Drop for RacyArc<T> {
    fn drop(&mut self) {
        // SAFETY: `ptr` is either `null` or a valid `Arc<T>` reference.
        self.ptr.with_mut(|ptr| {
            if !ptr.is_null() {
                unsafe {
                    Arc::from_raw(*ptr);
                }
            }
        });
    }
}
