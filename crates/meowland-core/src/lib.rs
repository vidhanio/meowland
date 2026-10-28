//! Shared errors, validated frame dimensions, and async runtime helpers.

#[cfg(feature = "async")]
pub mod asynchronous;

mod error;
pub mod future;
pub mod latency;
pub mod pixels;
pub mod scale;
#[cfg(feature = "async")]
pub mod signals;
pub mod timing;

pub use error::{Error, Result};
