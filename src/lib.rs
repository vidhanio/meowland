//! meowland: a Wayland compositor that draws each window inside a terminal
//! pane using the kitty graphics protocol.
//!
//! The binary is a thin shell around [`start`]. The modules are public so
//! process-level integration tests can speak the real protocols, the pane-side
//! encoder can be exercised directly, and the benchmarks can measure the
//! encoder and wire codec.

pub mod cli;
pub mod compositor;
pub mod diag;
mod error;
pub mod kitty;
pub mod protocol;
pub mod server;
pub mod terminal;

pub use cli::start;
pub use error::{Error, Result};
