//! Nonblocking terminal input with signed SGR pixel mouse decoding and
//! `vtparse`.

mod csi;
mod event;
#[cfg(feature = "fuzzing")]
pub mod fuzzing;
mod mouse;
mod parser;
mod reader;

pub use csi::number;
pub use event::{Event, Key, Kind};
pub use reader::Reader;
