//! Server child processes and their process groups.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use rustix::process::{Pid, PidfdFlags, Signal, kill_process_group, pidfd_open, pidfd_send_signal};

use crate::{Error, Result};

pub(super) fn launch_client(
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
    wayland: &str,
    token: &str,
) -> Result<Child> {
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
        .env("DESKTOP_STARTUP_ID", token);
    command
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
        .stderr(Stdio::inherit());
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| Error::io(format!("starting {}", executable.to_string_lossy()), error))
}

/// Retain process handles across escalation so orphaned helpers remain
/// reachable without signaling a reused PID.
pub(super) fn terminate_tree(roots: &HashSet<u32>) {
    if roots.is_empty() {
        return;
    }
    let mut processes = HashMap::new();
    for signal in [Signal::HUP, Signal::TERM, Signal::KILL] {
        for raw in descendants(roots).into_iter().chain(roots.iter().copied()) {
            if let std::collections::hash_map::Entry::Vacant(entry) = processes.entry(raw)
                && let Some(pid) = process_id(raw)
            {
                match pidfd_open(pid, PidfdFlags::empty()) {
                    Ok(process) => {
                        entry.insert(process);
                    }
                    Err(rustix::io::Errno::SRCH) => {}
                    Err(error) => {
                        tracing::warn!(pid = raw, %error, "cannot retain process for shutdown");
                    }
                }
            }
        }
        for process in processes.values() {
            let _ = pidfd_send_signal(process, signal);
        }
        for raw in roots {
            if let Some(group) = process_id(*raw) {
                let _ = kill_process_group(group, signal);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Reject zero or invalid PIDs before signaling.
fn process_id(raw: u32) -> Option<Pid> {
    i32::try_from(raw).ok().and_then(Pid::from_raw)
}

fn descendants(roots: &HashSet<u32>) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(rest) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
                continue;
            };
            if let Some(parent) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u32>().ok())
            {
                children.entry(parent).or_default().push(pid);
            }
        }
    }
    descendants_in(&children, roots)
}

fn descendants_in(children: &HashMap<u32, Vec<u32>>, roots: &HashSet<u32>) -> HashSet<u32> {
    let mut result = HashSet::new();
    let mut frontier: Vec<u32> = roots.iter().copied().collect();
    while let Some(parent) = frontier.pop() {
        if let Some(direct) = children.get(&parent) {
            for &pid in direct {
                if result.insert(pid) {
                    frontier.push(pid);
                }
            }
        }
    }
    result
}
