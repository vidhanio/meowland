//! Failures reported by the terminal-pane client.

use std::{io, path::PathBuf};

use thiserror::Error;

/// A pane client failure: a transport error or a missing terminal capability.
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying transport, signal, or I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The pane socket could not be reached.
    #[error(
        "could not reach the pane socket {} (is the server running?)",
        .path.display()
    )]
    Connect {
        /// Socket that was being connected.
        path: PathBuf,
        /// Original connection error.
        #[source]
        source: io::Error,
    },
    /// The terminal cannot report pixel-accurate pointer events.
    #[error("terminal does not support SGR pixel mouse reporting")]
    PixelMouseUnsupported,
    /// The terminal cannot edit Kitty graphics images in place.
    #[error(
        "terminal must support Kitty graphics with in-place frame updates (use Ghostty 1.4+ or current Kitty)"
    )]
    GraphicsUnsupported,
    /// The terminal cannot report the required keyboard transitions.
    #[error(
        "terminal must support Kitty keyboard disambiguation, event types, and all-key reporting"
    )]
    KeyboardUnsupported,
    /// The server did not answer the initial pane handshake in time.
    #[error("pane handshake timed out")]
    PaneHandshakeTimeout,
    /// Computed pane dimensions exceed the compositor's frame limits.
    #[error("pane dimensions {width}x{height} are outside the supported limits")]
    InvalidPaneSize {
        /// Width in physical pixels.
        width: u32,
        /// Height in physical pixels.
        height: u32,
    },
}

/// Result of a pane client operation.
pub type Result<T> = std::result::Result<T, Error>;
