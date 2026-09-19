//! meowland: a Wayland compositor that draws each window inside a terminal
//! pane using the kitty graphics protocol.
//!
//! The binary in `main.rs` is a thin shell around these modules.  They are
//! public so process-level integration tests can speak the real protocols and
//! so the pane-side encoder can be exercised directly.

pub mod compositor;
pub mod kitty;
pub mod protocol;
pub mod server;
pub mod terminal;
