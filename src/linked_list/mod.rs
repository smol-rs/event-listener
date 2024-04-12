//! Linked list of listeners.

#[cfg(feature = "std")]
mod std;
#[cfg(feature = "std")]
pub(crate) use std::*;

//#[cfg(not(feature = "std"))]
//mod no_std;
//#[cfg(not(feature = "std"))]
//pub(crate) use no_std::*;
