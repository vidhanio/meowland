//! Graceful client closure followed by process-tree escalation.

use std::time::{Duration, Instant};

use meowland_compositor::Command;
use rustix::process::Signal;

use super::{Server, notify, process::Termination};

const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

pub enum Shutdown {
    Running,
    Closing {
        deadline: Instant,
    },
    Escalating {
        signal: Signal,
        tree: Termination,
        deadline: Instant,
    },
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
            Self::Closing { deadline } | Self::Escalating { deadline, .. } => Some(*deadline),
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
            sd_notify::NotifyState::Status("Shutting down compositor and clients"),
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
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        let (signal, mut tree) = match std::mem::replace(&mut self.shutdown, Shutdown::Stopped) {
            Shutdown::Closing { .. } => {
                for task in self.connections.values() {
                    task.abort();
                }
                (Signal::HUP, Termination::new(self.children.take_groups()))
            }
            Shutdown::Escalating {
                signal: Signal::HUP,
                tree,
                ..
            } => (Signal::TERM, tree),
            Shutdown::Escalating {
                signal: Signal::TERM,
                tree,
                ..
            } => (Signal::KILL, tree),
            Shutdown::Escalating { .. } | Shutdown::Running | Shutdown::Stopped => return,
        };
        tree.signal(signal);
        self.shutdown = Shutdown::Escalating {
            signal,
            tree,
            deadline: now + SHUTDOWN_GRACE,
        };
    }

    pub fn terminate_children(&mut self) {
        let mut tree = match std::mem::replace(&mut self.shutdown, Shutdown::Stopped) {
            Shutdown::Escalating { tree, .. } => tree,
            _ => Termination::new(self.children.take_groups()),
        };
        tree.signal(Signal::KILL);
        self.children.reap();
    }
}
