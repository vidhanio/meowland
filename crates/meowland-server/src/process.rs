//! Application launch and asynchronous direct-child reaping.

use std::{
    ffi::OsString,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

use nix::sys::signal::{SigSet, Signal};

use super::Server;
use crate::{Error, Result};

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
        .env_remove("WAYLAND_SOCKET")
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

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use super::launch;

    #[test]
    fn launched_clients_discover_the_compositor_by_display_name() {
        let directory =
            std::env::temp_dir().join(format!("meowland-launch-env-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("create test directory");
        let output = directory.join("env");
        let args = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("env > \"$1\""),
            OsString::from("sh"),
            output.clone().into_os_string(),
        ];
        let env = [
            (OsString::from("WAYLAND_SOCKET"), OsString::from("3")),
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-0"),
            ),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ];

        let status = launch(&args, &env, &directory, "wayland-meowland-test", "token")
            .expect("launch shell")
            .wait()
            .expect("wait for shell");
        let environment = fs::read_to_string(&output).expect("read child environment");
        let _ = fs::remove_dir_all(&directory);

        assert!(status.success());
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with("WAYLAND_SOCKET=")),
            "{environment}"
        );
        assert!(
            environment
                .lines()
                .any(|line| line == "WAYLAND_DISPLAY=wayland-meowland-test"),
            "{environment}"
        );
    }
}
