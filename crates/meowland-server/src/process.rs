//! Direct-child launch and reaping; systemd owns process-tree containment.

use std::{
    ffi::OsString,
    io,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Child, Command, Stdio},
};

use meowland_core::{Error, Result};

/// Direct children retained until reaped, without supervising descendants.
#[derive(Default)]
pub struct Children {
    children: Vec<Child>,
}

impl Children {
    pub fn launch(
        &mut self,
        args: &[OsString],
        env: &[(OsString, OsString)],
        cwd: &Path,
        wayland: &str,
        token: &str,
    ) -> Result<()> {
        let Some(executable) = args.first() else {
            return Err(Error::RunRequiresCommand);
        };
        let mut command = Command::new(executable);
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
            .process_group(0);
        let child = command.spawn().map_err(|error| {
            Error::io(format!("starting {}", executable.to_string_lossy()), error)
        })?;
        self.children.push(child);
        Ok(())
    }

    pub fn reap(&mut self) {
        self.children.retain_mut(|child| {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        tracing::debug!(pid = child.id(), %status, "child exited");
                        return false;
                    }
                    Ok(None) => return true,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        tracing::warn!(pid = child.id(), %error, "cannot reap child process");
                        return true;
                    }
                }
            }
        });
    }
}
