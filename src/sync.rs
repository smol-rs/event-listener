//! The current implementation of synchronization primitives.

// TODO: Add implementations for portable_atomic and loom.

mod sync_impl {
    pub(crate) use alloc::sync::Arc;
    pub(crate) use core::cell;
    pub(crate) use core::sync::atomic;

    pub(crate) trait AtomicWithMut {
        type Target;

        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Target) -> R;
    }

    impl<T> AtomicWithMut for atomic::AtomicPtr<T> {
        type Target = *mut T;

        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Target) -> R,
        {
            f(self.get_mut())
        }
    }

    impl AtomicWithMut for atomic::AtomicUsize {
        type Target = usize;

        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Target) -> R,
        {
            f(self.get_mut())
        }
    }

    pub(crate) trait CellWithMut {
        type Target;

        fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*mut Self::Target) -> R;
    }

    impl<T> CellWithMut for cell::UnsafeCell<T> {
        type Target = T;

        fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*mut Self::Target) -> R,
        {
            f(self.get())
        }
    }
}

pub(crate) use sync_impl::*;
