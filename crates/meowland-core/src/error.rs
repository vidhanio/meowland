use std::{io, path::PathBuf};

use thiserror::Error;

/// Failures encountered while running the compositor or server.
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
