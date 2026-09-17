//! Starting a client program on this server's Wayland socket.

use std::{
    ffi::OsString,
    fs::File,
    process::{Child, Command, Stdio},
};

use crate::server::process;

/// Start a program as a client of this server.
///
/// Every client is told which Wayland socket to draw on, and the variables that
/// keep the toolkits from looking for an X display instead. The inherited
/// `DISPLAY` is passed on only when xwayland-satellite is there to answer it:
/// one left over from the session would send clients somewhere else.
///
/// The child's output goes to the log with the server's, not to a pane: a pane
/// shows one window, and the terminal in it belongs to meowland.
pub fn start(
    command: &[OsString],
    wayland_display: &str,
    x_display: Option<&str>,
    log: &File,
) -> std::io::Result<Child> {
    let Some((program, arguments)) = command.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no program to start",
        ));
    };
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "auto")
        .stdin(Stdio::null())
        .stdout(output(log))
        .stderr(output(log));
    match x_display {
        Some(display) => child.env("DISPLAY", display),
        None => child.env_remove("DISPLAY"),
    };
    // The server blocks the signals it watches, and a child inherits that mask.
    process::spawn_unblocked(&mut child);
    child.spawn()
}

/// Where a client's output goes, or nowhere if the log cannot be reached.
fn output(log: &File) -> Stdio {
    match log.try_clone() {
        Ok(file) => Stdio::from(file),
        Err(error) => {
            tracing::warn!(%error, "could not send a client's output to the log");
            Stdio::null()
        }
    }
}
