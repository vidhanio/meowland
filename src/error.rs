use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{action}: {source}")]
    IoContext {
        action: String,
        #[source]
        source: io::Error,
    },
    #[error("XDG_RUNTIME_DIR is not set")]
    RuntimeDirectoryUnset,
    #[error("XDG_RUNTIME_DIR is not a directory: {0}")]
    RuntimeDirectoryInvalid(PathBuf),
    #[error("a meowland server is already running ({0})")]
    ServerAlreadyListening(PathBuf),
    #[error("run requires a command")]
    RunRequiresCommand,
    #[error("terminal does not support SGR pixel mouse reporting")]
    PixelMouseUnsupported,
    #[error("terminal does not support kitty graphics")]
    GraphicsUnsupported,
    #[error("pane handshake timed out")]
    PaneHandshakeTimeout,
    #[error("server did not return windows")]
    UnexpectedWindowListResponse,
    #[error("server rejected the request: {0}")]
    ServerResponse(String),
}

impl Error {
    pub fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::IoContext {
            action: action.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
