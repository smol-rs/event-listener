//! Linked list of listeners.

#[cfg(feature = "std")]
mod mutex;
#[cfg(feature = "std")]
pub(crate) use mutex::*;

#[cfg(not(feature = "std"))]
mod lock_free;
#[cfg(not(feature = "std"))]
pub(crate) use lock_free::*;
