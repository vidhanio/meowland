//! Terminal event decoding and Kitty-to-evdev keyboard state.

mod input;
pub mod keyboard;

pub use input::{Event, Key, Kind, Reader, number};
