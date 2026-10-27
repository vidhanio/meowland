//! MIME-aware Kitty terminal clipboard transfers and deferred input actions.

mod action;
mod clipboard;

pub use action::{Action, Focus, Refresh};
pub use clipboard::Clipboard;
