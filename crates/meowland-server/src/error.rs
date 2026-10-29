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
    #[error("{action}: {source}")]
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

    /// Whether [`Display`](std::fmt::Display) already includes this error's
    /// immediate [`source`](std::error::Error::source).
    #[must_use]
    pub const fn source_is_in_display(&self) -> bool {
        matches!(self, Self::IoContext { .. })
    }

    pub(crate) fn report(&self) -> String {
        let mut report = self.to_string();
        let mut source = std::error::Error::source(self);
        if self.source_is_in_display() {
            source = source.and_then(std::error::Error::source);
        }
        while let Some(error) = source {
            let _ = write!(report, ": {error}");
            source = std::error::Error::source(error);
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
    fn report_renders_each_source_once() {
        let error = Error::IoContext {
            action: "binding the control socket".into(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        };

        assert_eq!(
            error.to_string(),
            "binding the control socket: permission denied"
        );
        assert_eq!(
            error.report(),
            "binding the control socket: permission denied"
        );
    }

    #[test]
    fn report_keeps_a_source_that_repeats_the_end_of_the_action() {
        let message = io::Error::from_raw_os_error(2).to_string();
        let error = Error::IoContext {
            action: format!("binding /tmp/{message}"),
            source: io::Error::from_raw_os_error(2),
        };

        assert_eq!(error.report(), format!("binding /tmp/{message}: {message}"));
    }
}
