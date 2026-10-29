//! Failures reported to the command-line user.

use std::io;

use thiserror::Error;

/// A command failed while talking to the server, the terminal, or the OS.
#[derive(Debug, Error)]
pub(crate) enum Error {
    /// A server-side or path helper failure.
    #[error(transparent)]
    Core(#[from] meowland_core::Error),
    /// A pane client failure.
    #[error(transparent)]
    Terminal(#[from] meowland_terminal::Error),
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
    pub(crate) fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Core(meowland_core::Error::io(action, source))
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::Core(source.into())
    }
}

/// Result of one meowland command.
pub(crate) type Result<T> = std::result::Result<T, Error>;
