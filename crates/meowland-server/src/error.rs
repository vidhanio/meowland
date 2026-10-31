//! Failures reported while starting or running the server.

use std::{fmt::Write as _, io, path::PathBuf};

use thiserror::Error;

/// A socket, launch, startup, or dispatch failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O failure without a more specific operation context.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// An I/O failure annotated with the operation that failed.
    #[error("{action}")]
    IoContext {
        /// Description of the attempted operation.
        action: String,
        /// Original I/O error, preserved for inspection.
        #[source]
        source: io::Error,
    },
    /// Another server already owns the requested socket.
    #[error("a meowland server is already running ({0})")]
    ServerAlreadyListening(PathBuf),
    /// A launch request contained no executable.
    #[error("run requires a command")]
    RunRequiresCommand,
}

impl Error {
    pub(crate) fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::IoContext {
            action: action.into(),
            source,
        }
    }

    pub(crate) fn with_context(self, action: impl Into<String>) -> Self {
        match self {
            Self::Io(source) => Self::io(action, source),
            Self::IoContext { .. } | Self::ServerAlreadyListening(_) | Self::RunRequiresCommand => {
                self
            }
        }
    }

    /// Render this error and each of its sources, separated by `": "`.
    pub(crate) fn report(&self) -> String {
        let mut report = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(error) = source {
            let _ = write!(report, ": {error}");
            source = error.source();
        }
        report
    }
}

/// Result of a server operation.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_renders_the_context_and_each_source_once() {
        let error = Error::io(
            "starting /bin/missing",
            io::Error::new(io::ErrorKind::NotFound, "no such file"),
        );

        assert_eq!(error.report(), "starting /bin/missing: no such file");
    }
}
