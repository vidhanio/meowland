//! Where meowland's own output goes.
//!
//! A server has no terminal of its own - the panes it is drawn on come and go,
//! and each is a socket rather than a file descriptor it can print to - so its
//! log is a file. The handle that comes back is also where the output of every
//! client it starts goes: a client that prints why it failed would otherwise
//! print into the screen it is being drawn on.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

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

/// Send logs to a file, and hand back the handle clients' own output goes to.
///
/// The file is emptied at startup and reopened for appending, so that every
/// writer - the server and every client it starts - writes at the end of it
/// and none of them can overwrite another's.
pub fn init(configured: Option<&Path>, level: Option<&str>) -> anyhow::Result<File> {
    let path = path(configured);
    File::create(&path).with_context(|| format!("could not log to {}", path.display()))?;
    let file = OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("could not log to {}", path.display()))?;
    let clients = file
        .try_clone()
        .with_context(|| format!("could not log to {}", path.display()))?;
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
