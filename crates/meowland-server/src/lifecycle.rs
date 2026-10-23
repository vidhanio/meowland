//! Deadline-driven server shutdown and process-tree escalation.

use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use rustix::process::Signal;

use super::{CompositorCommand, ControlResponse, Ordering, Server, Termination, notify};

const GRACE: Duration = Duration::from_millis(250);

pub enum Escalation {
    Hangup,
    Terminate,
    Kill,
}

pub enum Shutdown {
    Running,
    Closing,
    Escalating {
        stage: Escalation,
        tree: Termination,
    },
    Waiting,
    Stopped,
}

impl Shutdown {
    pub(super) const fn running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub(super) const fn stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }
}

impl Server {
    pub(super) fn compositor_gone(&mut self) {
        self.compositor_alive = false;
        match self.shutdown {
            Shutdown::Running => {
                self.compositor_failed = true;
                self.begin_shutdown();
            }
            Shutdown::Waiting => self.shutdown = Shutdown::Stopped,
            _ => {}
        }
    }

    pub(super) fn begin_shutdown(&mut self) {
        if !self.shutdown.running() {
            return;
        }
        self.shutdown = Shutdown::Closing;
        notify(&[
            sd_notify::NotifyState::Stopping,
            sd_notify::NotifyState::Status("Shutting down compositor and clients"),
        ]);
        if let Some((control, pane)) = &mut self.listeners {
            control.stop_accepting();
            pane.stop_accepting();
        }
        for (_, pending) in self.pending.drain() {
            self.handle.remove(pending.timer);
            pending.reply.waiting.store(false, Ordering::Relaxed);
            pending
                .reply
                .send(ControlResponse::Error("server stopping".into()));
        }
        let _ = self.commands.send(CompositorCommand::CloseAll);
        let ids: Vec<_> = self.panes.keys().copied().collect();
        for id in ids {
            self.release_pane(id, "server stopped".into());
        }
        if let Err(error) = self
            .handle
            .insert_source(Timer::from_duration(GRACE), |_, (), server| {
                server.advance_shutdown()
            })
        {
            tracing::error!(%error, "cannot schedule shutdown");
            self.force_shutdown();
        }
    }

    pub(super) fn force_shutdown(&mut self) {
        let mut tree = match std::mem::replace(&mut self.shutdown, Shutdown::Stopped) {
            Shutdown::Escalating { tree, .. } => tree,
            _ => Termination::new(std::mem::take(&mut self.child_groups)),
        };
        tree.signal(Signal::KILL);
        let _ = self.commands.send(CompositorCommand::Shutdown);
    }

    fn advance_shutdown(&mut self) -> TimeoutAction {
        self.shutdown = match std::mem::replace(&mut self.shutdown, Shutdown::Waiting) {
            Shutdown::Closing => {
                self.panes.clear();
                self.releasing_panes.clear();
                let mut tree = Termination::new(std::mem::take(&mut self.child_groups));
                tree.signal(Signal::HUP);
                Shutdown::Escalating {
                    stage: Escalation::Hangup,
                    tree,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Hangup,
                mut tree,
            } => {
                tree.signal(Signal::TERM);
                Shutdown::Escalating {
                    stage: Escalation::Terminate,
                    tree,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Terminate,
                mut tree,
            } => {
                tree.signal(Signal::KILL);
                Shutdown::Escalating {
                    stage: Escalation::Kill,
                    tree,
                }
            }
            Shutdown::Escalating {
                stage: Escalation::Kill,
                ..
            } => {
                let _ = self.commands.send(CompositorCommand::Shutdown);
                if self.compositor_alive {
                    Shutdown::Waiting
                } else {
                    Shutdown::Stopped
                }
            }
            other => other,
        };
        if matches!(self.shutdown, Shutdown::Escalating { .. }) {
            TimeoutAction::ToDuration(GRACE)
        } else {
            TimeoutAction::Drop
        }
    }
}
