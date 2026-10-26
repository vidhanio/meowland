//! Application launch and asynchronous direct-child reaping.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use meowland_core::{Error, Result};
use tokio::process::{Child, Command};

pub(crate) fn launch(
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
    wayland: &str,
    token: &str,
) -> Result<Child> {
    let Some(executable) = args.first() else {
        return Err(Error::RunRequiresCommand);
    };
    Command::new(executable)
        .args(&args[1..])
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .env("WAYLAND_DISPLAY", wayland)
        .env("XDG_ACTIVATION_TOKEN", token)
        .env("DESKTOP_STARTUP_ID", token)
        .env_remove("DISPLAY")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .env_remove("LISTEN_FDNAMES")
        .env_remove("WATCHDOG_PID")
        .env_remove("WATCHDOG_USEC")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("XDG_SESSION_TYPE", "wayland")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .map_err(|error| Error::io(format!("starting {}", executable.to_string_lossy()), error))
}

pub(crate) async fn wait(mut child: Child) {
    let pid = child.id();
    match child.wait().await {
        Ok(status) => tracing::debug!(?pid, %status, "child exited"),
        Err(error) => tracing::warn!(?pid, %error, "cannot reap child process"),
    }
}
