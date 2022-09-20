#[cfg(not(loom))]
mod sync_impl {
    #[cfg(not(feature = "portable-atomic"))]
    pub(crate) use core::sync::atomic;
    #[cfg(feature = "portable-atomic")]
    pub(crate) use portable_atomic as atomic;

    #[cfg(feature = "std")]
    pub(crate) use parking::{pair, Unparker};
    #[cfg(feature = "std")]
    pub(crate) use std::thread;

    pub(crate) trait AtomicWithMut {
        type Item;

        fn with_mut<R, F: FnOnce(&mut Self::Item) -> R>(&mut self, f: F) -> R;
    }

    impl AtomicWithMut for atomic::AtomicUsize {
        type Item = usize;

        fn with_mut<R, F: FnOnce(&mut Self::Item) -> R>(&mut self, f: F) -> R {
            f(self.get_mut())
        }
    }

    impl<T> AtomicWithMut for atomic::AtomicPtr<T> {
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
    pub(crate) use loom::thread;

    pub(crate) mod atomic {
        pub(crate) use core::sync::atomic::compiler_fence;
        pub(crate) use loom::sync::atomic::*;
    }

    use loom::sync::{Arc, Condvar, Mutex};

    /// Re-implementation of `parking::pair` based on loom.
    pub(crate) fn pair() -> (Parker, Unparker) {
        let inner = Arc::new(Inner {
            mutex: Mutex::new(false),
            condvar: Condvar::new(),
        });

        (Parker(inner.clone()), Unparker(inner))
    }

    /// Re-implementation of `parking::Parker` based on loom.
    pub(crate) struct Parker(Arc<Inner>);

    impl Parker {
        pub(crate) fn park(&self) {
            let mut state = self.0.mutex.lock().unwrap();

            loop {
                // If we haven't been notified, wait.
                if *state {
                    *state = false;
                    break;
                } else {
                    state = self.0.condvar.wait(state).unwrap();
                }
            }
        }

        // park_timeout is not available in loom
    }

    /// Re-implementation of `parking::Unparker` based on loom.
    #[derive(Clone)]
    pub(crate) struct Unparker(Arc<Inner>);

    impl Unparker {
        pub(crate) fn unpark(&self) {
            let mut state = self.0.mutex.lock().unwrap();
            *state = true;
            drop(state);

            self.0.condvar.notify_one();
        }
    }

    /// Internals of `Parker` and `Unparker.
    struct Inner {
        /// The mutex used to synchronize access to the state.
        mutex: Mutex<bool>,

        /// The condition variable used to notify the thread.
        condvar: Condvar,
    }

    #[allow(dead_code)]
    pub(crate) trait AtomicWithMut {}
}

pub(crate) use sync_impl::*;
