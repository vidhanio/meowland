//! Scoped nonblocking stdin for Crossterm's readiness-driven event reader.

use std::{io, os::fd::OwnedFd};

use rustix::{
    fs::{Mode, OFlags, open},
    io::fcntl_dupfd_cloexec,
    stdio::{dup2_stdin, stdin},
    termios::isatty,
};

/// Crossterm may read again after an incomplete or rejected escape sequence,
/// even inside `event::poll(Duration::ZERO)`. Its terminal descriptor must be
/// nonblocking so parsing cannot stall frame reception or acknowledgements.
///
/// Reopen, rather than dup and change flags: inherited stdin can share an open
/// file description with stdout, stderr, and the parent shell. Their writes
/// must remain blocking. Terminal attachment exclusively owns stdin while this
/// guard is alive, just as it owns the terminal's raw mode.
#[derive(Debug)]
pub(super) struct InputGuard {
    original: OwnedFd,
}

impl InputGuard {
    pub(super) fn enter() -> io::Result<Self> {
        let original = fcntl_dupfd_cloexec(stdin(), 3)?;
        let path = if isatty(stdin()) {
            "/proc/self/fd/0"
        } else {
            "/dev/tty"
        };
        let input = open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        dup2_stdin(&input)?;
        Ok(Self { original })
    }
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        if let Err(error) = dup2_stdin(&self.original) {
            tracing::warn!(%error, "Cannot restore terminal input descriptor");
        }
    }
}
