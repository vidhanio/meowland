//! Shared errors, validated frame dimensions, and signal guards.

#[cfg(feature = "async")]
pub mod asynchronous;

mod error;
pub mod pixels;
pub mod scale;
pub mod signals;

pub use error::{Error, Result};
