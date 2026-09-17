//! A Wayland compositor rendered through the kitty graphics protocol.
//!
//! The tree is cut along the three things this program is:
//!
//! - [`server`] is the server process: the panes attached to it, the control
//!   socket, the clients it started, and everything that outlives a terminal.
//! - [`wayland`] is the compositor proper: the Wayland clients, their state,
//!   and the frames they add up to. It runs on a thread of its own and talks to
//!   the server only in messages.
//! - [`client`] is one terminal pane: it hands over the terminal, sends what
//!   the user did, and writes the frames the server sends back.
//!
//! [`protocol`] is what they say to each other, and [`render`] and [`kitty`]
//! are what a frame is made of and how a terminal is told about it.

mod cli;
mod client;
mod dmabuf;
mod error;
mod keys;
mod kitty;
mod logging;
mod protocol;
mod render;
mod server;
mod wayland;

pub use error::Error;

/// Run the command line, and do what it says.
pub fn start() -> Result<(), Error> {
    cli::execute(cli::Cli::parse().action)
}
