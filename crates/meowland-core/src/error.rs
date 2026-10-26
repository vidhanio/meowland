use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Failures encountered while running the compositor, server, or terminal
/// client.
#[derive(Debug, Error)]
pub enum Error {
    /// An operating-system or stream operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// An I/O failure annotated with the operation that failed.
    #[error("{action}: {source}")]
    IoContext {
        /// Description of the attempted operation.
        action: String,
        /// Original I/O error, preserved for inspection.
        #[source]
        source: io::Error,
    },
    /// Socket discovery requires a runtime directory.
    #[error("XDG_RUNTIME_DIR is not set")]
    RuntimeDirectoryUnset,
    /// The configured runtime path is not a directory.
    #[error("XDG_RUNTIME_DIR is not a directory: {0}")]
    RuntimeDirectoryInvalid(PathBuf),
    /// Another server already owns the requested socket.
    #[error("a meowland server is already running ({0})")]
    ServerAlreadyListening(PathBuf),
    /// A launch request contained no executable.
    #[error("run requires a command")]
    RunRequiresCommand,
    /// The terminal cannot report pixel-accurate pointer events.
    #[error("terminal does not support SGR pixel mouse reporting")]
    PixelMouseUnsupported,
    /// The terminal cannot display Kitty image transfers.
    #[error("terminal does not support kitty graphics")]
    GraphicsUnsupported,
    /// The terminal cannot report the required keyboard transitions.
    #[error(
        "terminal must support Kitty keyboard disambiguation, event types, and all-key reporting"
    )]
    KeyboardUnsupported,
    /// The server did not answer the initial pane handshake in time.
    #[error("pane handshake timed out")]
    PaneHandshakeTimeout,
    /// A list request received a different response type.
    #[error("server did not return windows")]
    UnexpectedWindowListResponse,
    /// A successful launch response omitted its activation token.
    #[error("server did not return a launch activation token")]
    UnexpectedRunResponse,
    /// The server supplied a rejection reason.
    #[error("server rejected the request: {0}")]
    ServerResponse(String),
}

impl Error {
    /// Attach an operation description without discarding the original I/O
    /// error.
    pub fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::IoContext {
            action: action.into(),
            source,
        }
    }
}

/// Result of an application operation, retaining structured failure
/// information.
pub type Result<T> = std::result::Result<T, Error>;
