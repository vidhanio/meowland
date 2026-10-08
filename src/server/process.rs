//! Server child processes and their process groups.

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    os::unix::{net::UnixStream, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, PidfdFlags, Signal, kill_process_group, pidfd_open, pidfd_send_signal};

use crate::{Error, Result};

/// Xwayland owns its display lock and socket until it exits.
pub(super) struct Xwayland {
    pub(super) child: Child,
    pub(super) display: String,
}

pub(super) fn launch_client(args: &[OsString], wayland: &str, x11: Option<&str>) -> Result<Child> {
    let Some(executable) = args.first() else {
        return Err(Error::RunRequiresCommand);
    };
    let mut command = Command::new(executable);
    command.args(&args[1..]).env("WAYLAND_DISPLAY", wayland);
    match x11 {
        Some(display) => {
            command.env("DISPLAY", display);
        }
        None => {
            command.env_remove("DISPLAY");
        }
    }
    command
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "auto")
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
        // Detached helpers may leave the tree but remain in the process group.
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

/// Resolve `MEOWLAND_XWAYLAND`: `off` disables, `auto` searches PATH, otherwise
/// use a path.
pub(super) fn xwayland_program() -> Option<PathBuf> {
    match env::var("MEOWLAND_XWAYLAND").ok().as_deref().map(str::trim) {
        Some("off" | "0" | "false" | "no") => None,
        Some(path) if !matches!(path, "" | "auto" | "on" | "1" | "true" | "yes") => {
            Some(PathBuf::from(path))
        }
        _ => env::var_os("PATH").and_then(|path| {
            env::split_paths(&path)
                .map(|directory| directory.join("xwayland-satellite"))
                .find(|candidate| candidate.is_file())
        }),
    }
}

pub(super) fn start_xwayland(program: &Path, wayland: &str) -> Result<Xwayland> {
    // The satellite lacks `-displayfd`; retry display numbers when another
    // server wins the race.
    let mut last = String::new();
    for number in FIRST_X_DISPLAY..=LAST_X_DISPLAY {
        if x_display_in_use(number) {
            continue;
        }
        let mut child = Command::new(program)
            .arg(format!(":{number}"))
            .env("WAYLAND_DISPLAY", wayland)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
            .map_err(|error| Error::io(format!("starting {}", program.display()), error))?;
        match wait_for_x_socket(number, &mut child, Duration::from_secs(5)) {
            Ok(true) => {
                return Ok(Xwayland {
                    child,
                    display: format!(":{number}"),
                });
            }
            Ok(false) => {
                last = format!(":{number}");
                let _ = child.wait();
            }
            Err(error) => {
                // An unresponsive satellite is not a display-number collision.
                terminate_tree(&HashSet::from([child.id()]));
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    Err(Error::NoXDisplay { last })
}

const FIRST_X_DISPLAY: u32 = 8;
const LAST_X_DISPLAY: u32 = 63;

fn x_display_socket(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{number}"))
}

/// Another X server owns the display if its lock is held by a live process or
/// its socket exists.
fn x_display_in_use(number: u32) -> bool {
    if x_display_socket(number).exists() {
        return true;
    }
    let Ok(lock) = fs::read_to_string(format!("/tmp/.X{number}-lock")) else {
        return false;
    };
    let pid = lock.trim().parse::<i32>().ok();
    let Some(pid) = pid.and_then(Pid::from_raw) else {
        return true;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

fn wait_for_x_socket(number: u32, child: &mut Child, timeout: Duration) -> Result<bool> {
    let socket = x_display_socket(number);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(false);
        }
        if socket.exists() && UnixStream::connect(&socket).is_ok() {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(Error::XServerTimeout { socket, timeout })
}
