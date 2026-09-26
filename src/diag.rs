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
    sync::{Arc, atomic::AtomicBool},
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

/// A flag set by the signals that ask either the server or a pane to leave.
/// Keeping the signal set here prevents the two long-running processes from
/// quietly acquiring different shutdown behavior.
///
/// # Errors
/// Returns an error when a signal handler cannot be registered.
pub fn termination_flag() -> std::io::Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))?;
    }
    Ok(interrupted)
}
