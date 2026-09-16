//! Where meowland's own output goes.
//!
//! A server has no terminal of its own. The panes it draws on come and go, and
//! each is a socket rather than a file descriptor it can print to, so the log
//! is a file. The handle that `init` returns is also where the output of every
//! client goes. A client that prints why it failed would otherwise print into
//! the screen it is drawn on.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tracing_subscriber::EnvFilter;

/// Why the log could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The log file could not be created, reopened or copied for a client.
    #[error("could not log to {path}")]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A clock for a count that is reported once a second.
///
/// The counts belong to the caller: what a frame cost, what the presenter
/// spent. One second is a rate that a reader can divide in their head.
#[derive(Debug, Clone, Copy, Default)]
pub struct Report(Option<Instant>);

impl Report {
    /// The seconds since the last report, if a whole second has gone by.
    pub fn due(&mut self) -> Option<f64> {
        let now = Instant::now();
        let since = *self.0.get_or_insert(now);
        let elapsed = now - since;
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        self.0 = Some(now);
        Some(elapsed.as_secs_f64())
    }
}

/// Where logs go when the command line does not say.
pub fn path(configured: Option<&Path>) -> PathBuf {
    configured.map_or_else(
        || {
            Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
                .join("meowland.log")
        },
        Path::to_path_buf,
    )
}

/// Send logs to a file, and return the handle for the output of clients.
///
/// The file is emptied at startup and reopened for appending. Every writer, the
/// server and every client it starts, then writes at the end of the file, and
/// none can overwrite another.
pub fn init(configured: Option<&Path>, level: Option<&str>) -> Result<File, Error> {
    let path = path(configured);
    let at = |source| Error::File {
        path: path.clone(),
        source,
    };
    File::create(&path).map_err(at)?;
    let file = OpenOptions::new().append(true).open(&path).map_err(at)?;
    let clients = file.try_clone().map_err(at)?;
    let filter = level.map_or_else(
        || EnvFilter::new("meowland=info,warn"),
        |level| EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("meowland=info,warn")),
    );
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .init();
    tracing::info!(path = %path.display(), "logging to file");
    Ok(clients)
}
