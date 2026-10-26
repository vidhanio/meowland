//! Bounded Wayland-close grace before service-manager process cleanup.

use std::time::{Duration, Instant};

use meowland_compositor::Command;

use super::{Server, notify};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

pub enum Shutdown {
    Running,
    Closing { deadline: Instant },
    Stopped,
}

impl Shutdown {
    pub const fn running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub const fn stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }

    pub const fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Closing { deadline } => Some(*deadline),
            Self::Running | Self::Stopped => None,
        }
    }
}

impl Server {
    pub fn begin_shutdown(&mut self) {
        if !self.shutdown.running() {
            return;
        }
        self.shutdown = Shutdown::Closing {
            deadline: Instant::now() + SHUTDOWN_GRACE,
        };
        notify(&[
            sd_notify::NotifyState::Stopping,
            sd_notify::NotifyState::Status("Closing windows before service-manager cleanup"),
        ]);
        self.listeners.take();
        let _ = self.compositor.command(Command::CloseAll);
        for pane in self.panes.values_mut() {
            pane.release("server stopped".into());
        }
    }

    pub fn advance_shutdown(&mut self, now: Instant) {
        if self
            .shutdown
            .deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            self.shutdown = Shutdown::Stopped;
        }
    }
}
