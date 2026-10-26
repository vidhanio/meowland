//! Capability-selected terminal clipboard transports and deferred input
//! actions.

mod clipboard;
mod kitty;
mod osc52;

pub use clipboard::{Action, Clipboard, Focus, Protocol, Refresh};
