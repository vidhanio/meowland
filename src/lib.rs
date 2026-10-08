//! meowland: a Wayland compositor that draws each window inside a terminal
//! pane using the kitty graphics protocol.

mod clipboard;
pub mod compositor;
mod error;
pub mod kitty;
mod pixels;
pub mod protocol;
pub mod server;
pub mod signals;
pub mod terminal;

pub use error::{Error, Result};
