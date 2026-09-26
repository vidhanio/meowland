//! Child-process lifecycle for the server.
//!
//! Clients and Xwayland are launched in their own process groups so shutdown
//! can reliably reach helpers they leave behind. Keeping that policy beside
//! process discovery makes the ownership boundary explicit.

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

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use crate::{Error, Result, diag};

/// Rootless X11 clients, through `xwayland-satellite`.
///
/// Xwayland owns its display number for as long as it runs: it takes the
/// standard lock file and creates `/tmp/.X11-unix/X<n>`, and removes both on
/// exit. The satellite is told which number to use and the socket is watched
/// to prove the server came up.
pub(super) struct Xwayland {
    pub(super) child: Child,
    pub(super) display: String,
}

pub(super) fn launch_client(
    args: &[OsString],
    wayland: &str,
    x11: Option<&str>,
    log_path: &Path,
) -> Result<Child> {
    let Some(executable) = args.first() else {
        return Err(Error::RunRequiresCommand);
    };
    let log = diag::open(log_path)?;
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
        .stdout(log.try_clone()?)
        .stderr(log);
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| Error::io(format!("starting {}", executable.to_string_lossy()), error))
}

/// Signal every process in the trees rooted at `roots`, and the process group
/// of each root.
///
/// The tree is rescanned before each signal, because Unix does not take
/// children down with their parent: a helper that forked on the way out would
/// otherwise be missed by every signal. Escalation is `SIGHUP`, `SIGTERM`,
/// `SIGKILL`, with a moment between them for a process to leave on its own
/// terms.
pub(super) fn terminate_tree(roots: &HashSet<u32>) {
    for signal in [Signal::HUP, Signal::TERM, Signal::KILL] {
        for raw in descendants(roots).into_iter().chain(roots.iter().copied()) {
            if let Some(pid) = process_id(raw) {
                let _ = kill_process(pid, signal);
            }
        }
        // Every child was started in its own process group, so the group goes
        // too: a daemonised grandchild that left the tree is still in it.
        for raw in roots {
            if let Some(group) = process_id(*raw) {
                let _ = kill_process_group(group, signal);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// `Pid::from_raw` refuses 0 and negatives, so nothing here can signal a group
/// by mistake.
fn process_id(raw: u32) -> Option<Pid> {
    i32::try_from(raw).ok().and_then(Pid::from_raw)
}

fn descendants(roots: &HashSet<u32>) -> HashSet<u32> {
    let mut parents = HashMap::new();
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
                parents.insert(pid, parent);
            }
        }
    }
    descendants_in(&parents, roots)
}

fn descendants_in(parents: &HashMap<u32, u32>, roots: &HashSet<u32>) -> HashSet<u32> {
    let mut result = HashSet::new();
    let mut frontier: Vec<u32> = roots.iter().copied().collect();
    while let Some(parent) = frontier.pop() {
        for (&pid, &ppid) in parents {
            if ppid == parent && result.insert(pid) {
                frontier.push(pid);
            }
        }
    }
    result
}

/// Where `xwayland-satellite` comes from.
///
/// `MEOWLAND_XWAYLAND` may disable it (`off`), force the normal lookup
/// (`auto`, the default) or name a binary to use.
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

pub(super) fn start_xwayland(program: &Path, wayland: &str, log_path: &Path) -> Result<Xwayland> {
    let log = diag::open(log_path)?;
    // `xwayland-satellite` forwards only a subset of Xwayland's options and
    // does not accept `-displayfd`, so the display number is chosen here and
    // verified by waiting for the socket. A number that loses a race makes
    // Xwayland exit at once; the next candidate is then tried.
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
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
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
                // Nothing is listening but the process is alive: this is a
                // broken satellite, not a lost race, so stop trying numbers
                // and take its tree down with it.
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
        if let Ok(Some(_)) = child.try_wait() {
            return Ok(false);
        }
        if socket.exists() && UnixStream::connect(&socket).is_ok() {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(Error::XServerTimeout { socket, timeout })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_descendants_include_every_generation_and_exclude_other_trees() {
        let parents = HashMap::from([(2, 1), (3, 2), (4, 2), (5, 99)]);
        assert_eq!(
            descendants_in(&parents, &HashSet::from([1])),
            HashSet::from([2, 3, 4])
        );
    }
}
