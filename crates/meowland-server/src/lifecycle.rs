//! Deadline-driven server shutdown and process-tree escalation.

use std::time::{Duration, Instant};

use rustix::process::Signal;

use super::{CompositorCommand, Server, Termination, notify};

const GRACE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositorStatus {
    Running,
    Failed,
    Exited,
}

impl CompositorStatus {
    pub const fn alive(self) -> bool {
        matches!(self, Self::Running)
    }

    pub const fn failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

pub enum Escalation {
    Hangup,
    Terminate,
    Kill,
}

pub enum Shutdown {
    Running,
    Closing {
        deadline: Instant,
    },
    Escalating {
        stage: Escalation,
        tree: Termination,
        deadline: Instant,
    },
    Waiting,
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
            _ => None,
        }
    }
}

impl Server {
    pub fn compositor_gone(&mut self) {
        match self.shutdown {
            Shutdown::Running => {
                self.compositor = CompositorStatus::Failed;
                self.begin_shutdown();
            }
            Shutdown::Waiting => {
                self.compositor = CompositorStatus::Exited;
                self.shutdown = Shutdown::Stopped;
            }
            Shutdown::Closing { .. } | Shutdown::Escalating { .. } | Shutdown::Stopped => {
                self.compositor = CompositorStatus::Exited;
            }
        }
    }

    pub fn begin_shutdown(&mut self) {
        if !self.shutdown.running() {
            return;
        }
        self.shutdown = Shutdown::Closing {
            deadline: Instant::now() + GRACE,
        };
        notify(&[
            sd_notify::NotifyState::Stopping,
            sd_notify::NotifyState::Status("Shutting down compositor and clients"),
        ]);
        self.listeners.take();
        self.handshakes.abort_all();
        self.requests.close_all("server stopping");
        let _ = self.commands.send(CompositorCommand::CloseAll);
        let ids: Vec<_> = self.panes.keys().copied().collect();
        for id in ids {
            self.release_pane(id, "server stopped".into());
        }
    }

    pub fn force_shutdown(&mut self) {
        let mut tree = match std::mem::replace(&mut self.shutdown, Shutdown::Stopped) {
            Shutdown::Escalating { tree, .. } => tree,
            _ => Termination::new(self.children.take_groups()),
        };
        tree.signal(Signal::KILL);
        self.requests.abandon_all();
        self.controls.abort_all();
        self.handshakes.abort_all();
        self.connections.abort_all();
        let _ = self.commands.send(CompositorCommand::Shutdown);
    }

    pub fn advance_shutdown(&mut self) {
        let deadline = Instant::now() + GRACE;
        self.shutdown = match std::mem::replace(&mut self.shutdown, Shutdown::Waiting) {
            Shutdown::Closing { .. } => {
                self.controls.abort_all();
                self.panes.clear();
                self.connections.abort_all();
                let mut tree = Termination::new(self.children.take_groups());
                tree.signal(Signal::HUP);
                Shutdown::Escalating {
                    stage: Escalation::Hangup,
                    tree,
                    deadline,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Hangup,
                mut tree,
                ..
            } => {
                tree.signal(Signal::TERM);
                Shutdown::Escalating {
                    stage: Escalation::Terminate,
                    tree,
                    deadline,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Terminate,
                mut tree,
                ..
            } => {
                tree.signal(Signal::KILL);
                Shutdown::Escalating {
                    stage: Escalation::Kill,
                    tree,
                    deadline,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Kill,
                ..
            } => {
                let _ = self.commands.send(CompositorCommand::Shutdown);
                if self.compositor.alive() {
                    Shutdown::Waiting
                } else {
                    Shutdown::Stopped
                }
            }
            other => other,
        };
    }
}
