//! Shared errors, validated frame dimensions, scale, refresh rate, and latency
//! statistics.

mod error;
pub mod latency;
pub mod pixels;
pub mod scale;
pub mod timing;

pub use error::{Error, Result};
