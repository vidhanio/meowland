//! Terminal event decoding and Kitty key tracking.

mod input;
pub mod keyboard;

#[cfg(feature = "fuzzing")]
pub use input::fuzzing;
pub use input::{Event, Focus, Key, Kind, Reader, number};
