//! The one place the server and the panes write diagnostics.
//!
//! A pane's terminal is its display, so its own diagnostics cannot go there;
//! both sides append to the same file: `MEOWLAND_LOG`, or `meowland.log`
//! beside the sockets.  Keeping the path and the line format here is what
//! makes the two sides agree on the file.

use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// The log file that goes with `socket`.
#[must_use]
pub fn path(socket: &Path) -> PathBuf {
    std::env::var_os("MEOWLAND_LOG").map_or_else(
        || {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("meowland.log")
        },
        PathBuf::from,
    )
}

/// Open the log for appending.
///
/// # Errors
/// Returns an error when the file cannot be created or opened.
pub fn open(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Seconds since the epoch, which is what every line is stamped with.
#[must_use]
pub fn seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Append one stamped line, ignoring failures: a log that cannot be written
/// is never a reason for the server to stop.
pub fn line(path: &Path, message: &str) {
    if let Ok(mut file) = open(path) {
        let _ = writeln!(file, "{} {message}", seconds());
    }
}
