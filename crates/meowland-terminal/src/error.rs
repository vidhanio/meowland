//! Failures reported by the terminal-pane client.

use std::io;

use thiserror::Error;

/// A pane client failure: an underlying transport error or a missing terminal
/// capability.
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying transport, signal, or I/O failure.
    #[error(transparent)]
    Core(#[from] meowland_core::Error),
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
}

impl Error {
    /// Attach an operation description without discarding the original I/O
    /// error.
    pub fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Core(meowland_core::Error::io(action, source))
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::Core(source.into())
    }
}

/// Result of a pane client operation.
pub type Result<T> = std::result::Result<T, Error>;
