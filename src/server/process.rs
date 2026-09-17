//! Process tree discovery and shutdown.

use std::{
    collections::HashMap,
    os::unix::process::CommandExt as _,
    process::Command,
    time::{Duration, Instant},
};

use nutype::nutype;
use rustix::{
    process::{Pid, Signal, kill_process, test_kill_process},
    runtime::{How, KernelSigSet, kernel_sigprocmask},
};

/// A process ID used while walking and stopping the server's process tree.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct ProcessId(u32);

/// How long each signal is given before the next one is sent.
///
/// Long enough for a client to close its windows and exit, and short enough
/// that stopping a server feels immediate.
const GRACE: Duration = Duration::from_millis(250);

/// How often the tree is checked while a signal is being waited out.
const POLL: Duration = Duration::from_millis(20);

/// A signal, and the grace that follows it.
pub const ESCALATION: [(Signal, Duration); 3] = [
    (Signal::HUP, GRACE),
    (Signal::TERM, GRACE),
    (Signal::KILL, GRACE),
];

/// Start `command` with the signal mask of this process cleared in the child.
///
/// The server blocks the signals that it watches, and a signal mask is
/// inherited across `fork` and `exec`. Without this, a client of the server
/// never sees a `SIGHUP` or a `SIGTERM` from anywhere: not from the escalation
/// above, and not from a `kill` typed at a shell. The mask is cleared between
/// the fork and the exec, which is the last moment before the program replaces
/// the process and the only place where the mask of the child can still be
/// changed.
///
/// Not used for the server itself, which needs the block for its own signal
/// handling, and not for threads, which do not exec.
pub fn spawn_unblocked(command: &mut Command) -> &mut Command {
    // SAFETY: the closure runs between `fork` and `exec`, where only
    // async-signal-safe calls are allowed. `rt_sigprocmask` is one, its
    // argument is a mask by value, and nothing else happens here.
    #[expect(
        unsafe_code,
        reason = "a child's signal mask can only be changed between fork and exec, which is the contract of pre_exec"
    )]
    unsafe {
        command.pre_exec(|| {
            kernel_sigprocmask(How::SETMASK, Some(&KernelSigSet::empty()))
                .map(|_| ())
                .map_err(std::io::Error::from)
        })
    }
}
/// Start `command` in a new session.
///
/// The session leader check in [`is_detached`] identifies the child that should
/// enter the server event loop instead of spawning another copy.
pub fn spawn_detached(command: &mut Command) -> &mut Command {
    // SAFETY: the closure runs between `fork` and `exec`, where only
    // async-signal-safe calls are allowed. `setsid` is one, and no allocation
    // or Rust runtime work happens here.
    #[expect(
        unsafe_code,
        reason = "a detached child can only create its session between fork and exec"
    )]
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        })
    }
}

/// Whether this process is the leader of its own session.
pub fn is_detached() -> bool {
    rustix::process::getsid(None).is_ok_and(|session| session == rustix::process::getpid())
}

/// Every process under `root`, not including `root` itself.
///
/// The tree is read in one pass over `/proc`, then walked from `root`.
pub fn descendants_of(root: ProcessId) -> Vec<ProcessId> {
    let mut children: HashMap<ProcessId, Vec<ProcessId>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .map(ProcessId::new)
        else {
            continue;
        };
        let Some(parent) = parent_of(pid) else {
            continue;
        };
        children.entry(parent).or_default().push(pid);
    }

    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for child in children.remove(&pid).unwrap_or_default() {
            found.push(child);
            frontier.push(child);
        }
    }
    found
}

/// The parent of one process, read from `/proc/<pid>/stat`.
///
/// The second field of that file is the name of the process in parentheses, and
/// the name may hold spaces and parentheses, so the fields are counted from the
/// last `)`.
fn parent_of(pid: ProcessId) -> Option<ProcessId> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.get(stat.rfind(')')? + 2..)?;
    fields
        .split_whitespace()
        .nth(1)?
        .parse::<u32>()
        .ok()
        .map(ProcessId::new)
}

/// Send `signal` to every process in `pids`, and to nothing else.
pub fn signal(pids: &[ProcessId], signal: Signal) {
    for pid in pids {
        let Some(pid) = Pid::from_raw(pid.into_inner() as i32) else {
            continue;
        };
        if let Err(err) = kill_process(pid, signal) {
            tracing::debug!(
                pid = pid.as_raw_pid(),
                ?signal,
                ?err,
                "could not signal a process"
            );
        }
    }
}

/// Whether one process is still there.
///
/// A process that this server started and has not reaped yet counts as there,
/// because it still holds the pid.
pub fn alive(pid: ProcessId) -> bool {
    Pid::from_raw(pid.into_inner() as i32).is_some_and(|pid| test_kill_process(pid).is_ok())
}

/// Wait until every process in `pids` is gone, or `grace` runs out.
///
/// `reap` is called between checks, so that the children of this process are
/// taken back from the kernel while their own children are being waited for.
pub fn wait_until_gone(
    pids: &mut Vec<ProcessId>,
    grace: Duration,
    reap: &mut impl FnMut(),
) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        reap();
        pids.retain(|pid| alive(*pid));
        if pids.is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::{process::Command, thread};

    use super::*;

    #[test]
    fn a_tree_is_read_and_stopped_in_full() {
        // A client with a helper of its own: the helper is not a child of this
        // process, and it is what a per-child kill would leave behind.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & sleep 30")
            .spawn()
            .expect("could not start a shell");
        thread::sleep(Duration::from_millis(200));

        let root = ProcessId::new(std::process::id());
        let child_pid = ProcessId::new(child.id());
        let found = descendants_of(root);
        assert!(
            found.contains(&child_pid),
            "the shell is under this process: {found:?}"
        );
        for pid in &found {
            assert_ne!(*pid, root, "the root is not its own descendant");
        }
        let sleeps = found
            .iter()
            .filter(|pid| **pid != child_pid && is_sleep(**pid))
            .count();
        assert!(sleeps >= 2, "both helpers are in the tree: {found:?}");

        // The helpers outlive their parent, which is what the escalation is
        // for.
        signal(&found, Signal::KILL);
        let mut left = found;
        assert!(
            // The shell is this process's own child and stays a zombie until
            // it is reaped, which is done here as the server does it.
            wait_until_gone(&mut left, Duration::from_secs(2), &mut || {
                let _ = child.try_wait();
            }),
            "nothing is left running: {left:?}"
        );
        let _ = child.wait();
    }

    /// Whether a process is one of the `sleep` helpers, by its command name.
    ///
    /// The name is the second field of `/proc/<pid>/stat`, in parentheses, and
    /// it may hold spaces and parentheses of its own.
    fn is_sleep(pid: ProcessId) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.split_once(')').is_some_and(|(head, _)| {
                head.rsplit_once('(')
                    .is_some_and(|(_, comm)| comm == "sleep")
            })
        })
    }
}

#[cfg(test)]
mod signal_tests {
    use std::{process::Command, time::Duration};

    use super::*;

    #[test]
    fn a_signal_stops_a_process() {
        let mut child = Command::new("sleep").arg("30").spawn().expect("sleep");
        let pid = ProcessId::new(child.id());
        signal(&[pid], Signal::HUP);
        let mut left = vec![pid];
        let gone = wait_until_gone(&mut left, Duration::from_secs(2), &mut || {
            let _ = child.try_wait();
        });
        assert!(gone, "SIGHUP stopped sleep: {left:?} alive={}", alive(pid));
    }
}
