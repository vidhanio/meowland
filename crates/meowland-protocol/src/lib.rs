//! Shared control and pane messages, clipboard data, and socket discovery.

#[cfg(feature = "async")]
pub mod asynchronous;

pub mod clipboard;
mod paths;
mod protocol;

pub use paths::Paths;
pub use protocol::*;
