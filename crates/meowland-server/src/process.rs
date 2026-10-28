//! Application launch and asynchronous direct-child reaping.

use std::{
    ffi::OsString,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

use meowland_core::{Error, Result};
use nix::sys::signal::{SigSet, Signal};

use super::Server;

/// Launch with the server's signalfd subscriptions unblocked in the child.
/// The pre-exec closure only invokes the async-signal-safe signal-mask
/// operation on a set built before fork, without allocating or accessing shared
/// state.
#[expect(
    clippy::undocumented_unsafe_blocks,
    reason = "the pre-exec safety contract is documented above"
)]
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
    let mut unblocked = SigSet::empty();
    for signal in [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ] {
        unblocked.add(signal);
    }
    let mut command = Command::new(executable);
    unsafe {
        command.pre_exec(move || unblocked.thread_unblock().map_err(std::io::Error::from));
    }
    command
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

impl Server {
    pub(crate) fn reap_children(&mut self) {
        self.children.retain_mut(|child| {
            let pid = child.id();
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::debug!(pid, %status, "child exited");
                    false
                }
                Ok(None) => true,
                Err(error) => {
                    tracing::warn!(pid, %error, "cannot reap child process");
                    true
                }
            }
        });
    }
}
