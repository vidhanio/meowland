//! Terminal event decoding and Kitty key tracking.

mod input;
pub mod keyboard;

pub use input::{Event, Key, Kind, Reader, number};
