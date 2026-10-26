//! Runtime-managed termination signals shared by the server and terminal.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};

/// Unix signal requesting shutdown of the current session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationSignal {
    /// Interactive interrupt (`SIGINT`).
    Interrupt,
    /// Service termination (`SIGTERM`).
    Terminate,
    /// Controlling terminal disconnected (`SIGHUP`).
    Hangup,
}

/// Termination subscriptions owned by one server or terminal session.
#[derive(Debug)]
pub struct Termination {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl Termination {
    /// Subscribe on the current Tokio runtime before starting session work.
    ///
    /// # Errors
    /// Returns an error if a signal subscription cannot be installed.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Wait for termination or hangup, retaining pending signals on
    /// cancellation.
    pub async fn recv(&mut self) -> TerminationSignal {
        tokio::select! {
            _ = self.interrupt.recv() => TerminationSignal::Interrupt,
            _ = self.terminate.recv() => TerminationSignal::Terminate,
            _ = self.hangup.recv() => TerminationSignal::Hangup,
        }
    }
}
