#[cfg(not(loom))]
mod sync_impl {
    pub(crate) use core::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};

    #[cfg(feature = "std")]
    pub(crate) use parking::{pair, Unparker};

    pub(crate) trait AtomicWithMut {
        type Item;

        fn with_mut<R, F: FnOnce(&mut Self::Item) -> R>(&mut self, f: F) -> R;
    }

    impl AtomicWithMut for AtomicUsize {
        type Item = usize;

        fn with_mut<R, F: FnOnce(&mut Self::Item) -> R>(&mut self, f: F) -> R {
            f(self.get_mut())
        }
    }

    impl<T> AtomicWithMut for AtomicPtr<T> {
        type Item = *mut T;

        fn with_mut<R, F: FnOnce(&mut Self::Item) -> R>(&mut self, f: F) -> R {
            f(self.get_mut())
        }
    }

    pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(crate) const fn new(item: T) -> Self {
            Self(core::cell::UnsafeCell::new(item))
        }

        pub(crate) fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*mut T) -> R,
        {
            f(self.0.get())
        }
    }
}

#[cfg(loom)]
mod sync_impl {
    pub(crate) use loom::cell::UnsafeCell;
    pub(crate) use loom::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};

    /// Re-implementation of `parking::pair` based on loom.
    pub(crate) fn pair() -> (Parker, Unparker) {
        let th = loom::thread::current();

        (Parker(Default::default()), Unparker(th))
    }

    /// Re-implementation of `parking::Parker` based on loom.
    pub(crate) struct Parker(core::marker::PhantomData<*mut ()>);

    impl Parker {
        pub(crate) fn park(&self) {
            loom::thread::park();
        }

        // park_timeout is not available in loom
    }

    /// Re-implementation of `parking::Unparker` based on loom.
    #[derive(Clone)]
    pub(crate) struct Unparker(loom::thread::Thread);

    impl Unparker {
        pub(crate) fn unpark(&self) {
            self.0.unpark();
        }
    }

    #[allow(dead_code)]
    pub(crate) trait AtomicWithMut {}
}

pub(crate) use sync_impl::*;
