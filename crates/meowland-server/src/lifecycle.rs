//! Bounded Wayland-close grace and pane drain for standalone and managed
//! servers.

use std::time::{Duration, Instant};

use meowland_compositor::Command;
use meowland_protocol::REQUEST_TIMEOUT;

use super::{Server, notify};

/// Time windows get to close before the server may stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// Longest wait for released panes to receive their remaining output.
///
/// Each pane writer gets [`REQUEST_TIMEOUT`] to drain once released; the
/// margin lets a writer that uses its whole budget finish before the server
/// stops and aborts it.
const DRAIN_DEADLINE: Duration = REQUEST_TIMEOUT.saturating_add(Duration::from_millis(500));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shutdown {
    Running,
    /// Windows were asked to close and panes were released.
    Closing {
        /// Until then, windows may still close cleanly.
        grace: Instant,
        /// Stop even if connections are still draining.
        deadline: Instant,
    },
    Stopped,
}

impl Shutdown {
    pub(crate) const fn running(self) -> bool {
        matches!(self, Self::Running)
    }

    pub(crate) const fn stopped(self) -> bool {
        matches!(self, Self::Stopped)
    }

    fn close(now: Instant) -> Self {
        Self::Closing {
            grace: now + SHUTDOWN_GRACE,
            deadline: now + DRAIN_DEADLINE,
        }
    }

    /// Next time shutdown progress depends on the clock.
    pub(crate) fn wakeup(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Closing { grace, .. } if now < grace => Some(grace),
            Self::Closing { deadline, .. } => Some(deadline),
            Self::Running | Self::Stopped => None,
        }
    }

    /// Stop once the grace has passed and every connection finished, or at
    /// the drain deadline.
    fn advance(&mut self, now: Instant, open_connections: usize) {
        if let Self::Closing { grace, deadline } = *self
            && (now >= deadline || (now >= grace && open_connections == 0))
        {
            *self = Self::Stopped;
        }
    }
}

impl Server {
    pub(crate) fn begin_shutdown(&mut self) {
        if !self.shutdown.running() {
            return;
        }
        self.shutdown = Shutdown::close(Instant::now());
        notify(&[
            sd_notify::NotifyState::Stopping,
            sd_notify::NotifyState::Status("Closing windows and releasing panes"),
        ]);
        for listener in self.listeners.drain(..) {
            self.handle.remove(listener.token);
        }
        self.compositor.handle_command(Command::CloseAll);
        for connection in self.connections.values_mut() {
            connection.release("server stopped");
        }
    }

    pub(crate) fn advance_shutdown(&mut self, now: Instant) {
        self.shutdown.advance(now, self.connections.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_waits_for_draining_connections_until_the_deadline() {
        let start = Instant::now();
        let mut shutdown = Shutdown::close(start);
        shutdown.advance(start + SHUTDOWN_GRACE, 1);
        assert!(!shutdown.stopped(), "a pane is still draining");
        shutdown.advance(start + REQUEST_TIMEOUT, 1);
        assert!(
            !shutdown.stopped(),
            "a writer may use its whole drain budget"
        );
        shutdown.advance(start + DRAIN_DEADLINE, 1);
        assert!(shutdown.stopped());
    }

    #[test]
    fn shutdown_keeps_the_window_grace_even_when_drained() {
        let start = Instant::now();
        let mut shutdown = Shutdown::close(start);
        shutdown.advance(start, 0);
        assert!(!shutdown.stopped());
        shutdown.advance(start + SHUTDOWN_GRACE, 0);
        assert!(shutdown.stopped());
    }
}
