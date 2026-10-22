//! Shared signal handling for the server and terminal pane.

use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use signal_hook::SigId;

/// Signal registrations that wake a readiness loop without masking child
/// signals.
#[derive(Debug)]
pub(crate) struct Wakeup {
    socket: UnixStream,
    handlers: Vec<SigId>,
}

impl Wakeup {
    pub(crate) fn new(signals: &[i32]) -> std::io::Result<Self> {
        let (socket, writer) = UnixStream::pair()?;
        socket.set_nonblocking(true)?;
        let mut wakeup = Self {
            socket,
            handlers: Vec::new(),
        };
        for &signal in signals {
            wakeup.handlers.push(signal_hook::low_level::pipe::register(
                signal,
                writer.try_clone()?,
            )?);
        }
        Ok(wakeup)
    }

    pub(crate) fn drain(&self) {
        let mut bytes = [0; 256];
        while rustix::io::read(&self.socket, &mut bytes).is_ok_and(|read| read > 0) {}
    }
}

impl std::os::fd::AsFd for Wakeup {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Drop for Wakeup {
    fn drop(&mut self) {
        for handler in &self.handlers {
            signal_hook::low_level::unregister(*handler);
        }
    }
}

/// Termination handlers owned by one server or pane invocation.
#[derive(Debug)]
pub struct TerminationFlag {
    flag: Arc<AtomicBool>,
    handlers: [Option<SigId>; 3],
}

impl TerminationFlag {
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Drop for TerminationFlag {
    fn drop(&mut self) {
        for handler in self.handlers.iter().flatten() {
            signal_hook::low_level::unregister(*handler);
        }
    }
}

/// Register termination handlers until the returned guard is dropped.
///
/// # Errors
/// Returns an error when a signal handler cannot be registered.
pub fn termination_flag() -> std::io::Result<TerminationFlag> {
    register_termination(true)
}

/// Register server termination handlers, ignoring SIGHUP because there is no
/// reloadable configuration. Terminal panes still treat hangup as termination.
///
/// # Errors
/// Returns an error when a signal handler cannot be registered.
pub fn server_termination_flag() -> std::io::Result<TerminationFlag> {
    register_termination(false)
}

fn register_termination(hangup: bool) -> std::io::Result<TerminationFlag> {
    let mut guard = TerminationFlag {
        flag: Arc::new(AtomicBool::new(false)),
        handlers: [None; 3],
    };
    for (slot, signal) in guard.handlers.iter_mut().zip([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ]) {
        let flag = if signal == signal_hook::consts::SIGHUP && !hangup {
            Arc::new(AtomicBool::new(false))
        } else {
            Arc::clone(&guard.flag)
        };
        *slot = Some(signal_hook::flag::register(signal, flag)?);
    }
    Ok(guard)
}
