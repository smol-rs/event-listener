//! Implementation of synchronization primitives.

// TODO: portable_atomic or loom implementations

#[cfg(not(all(feature = "loom", loom)))]
mod sync_impl {
    pub use alloc::sync::Arc;
    pub use core::cell;
    pub use core::sync::atomic;
}

#[cfg(all(feature = "loom", loom))]
mod sync_impl {
    pub use loom::cell;
    pub use loom::sync::atomic;
    pub use loom::sync::Arc;
}

pub use sync_impl::*;
