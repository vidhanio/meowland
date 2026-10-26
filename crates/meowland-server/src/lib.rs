//! Socket I/O and process ownership, with rendering isolated on its own thread.

use std::collections::HashMap;
use std::io;

use meowland_compositor::{Command, Event, PaneId};
use meowland_core::Result;
use meowland_core::scale::UiScale;
use meowland_protocol::{ControlRequest, ControlResponse, Hello, PaneAction, Paths, ServerToPane};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

mod actor;
mod compositor;
mod control;
mod lifecycle;
mod process;
mod sockets;
mod transport;

use compositor::DisplayThread;
use lifecycle::Shutdown;
use sockets::Listeners;
use transport::PaneTransport;

/// One socket budget, with the last slot reserved for control traffic.
const MAX_CONNECTIONS: usize = 256;

type IncomingSender = mpsc::Sender<Incoming>;

enum Incoming {
    Control(ControlRequest, oneshot::Sender<ControlResponse>),
    Attach(PaneId, Hello, PaneTransport),
    PaneMessage(PaneId, PaneAction),
}

struct Server {
    compositor: DisplayThread,
    incoming: IncomingSender,
    messages: mpsc::Receiver<Incoming>,
    listeners: Option<Listeners>,
    connections: HashMap<PaneId, AbortHandle>,
    tasks: JoinSet<(PaneId, io::Result<()>)>,
    controls: JoinSet<control::Completion>,
    panes: HashMap<PaneId, PaneTransport>,
    next_connection: u64,
    children: JoinSet<()>,
    shutdown: Shutdown,
}

/// Run the compositor server with default UI scaling.
///
/// # Errors
/// Returns an error if startup, socket I/O or compositor dispatch fails.
pub async fn serve(paths: &Paths) -> Result<()> {
    serve_scaled(paths, UiScale::default()).await
}

/// Run the server with a shared UI scale for all terminal panes.
///
/// Binds the supplied paths unless listeners are provided by socket activation.
/// Shutdown gives clients a bounded Wayland-close grace period. With the
/// systemd service, its cgroup also cleans up remaining client processes;
/// standalone operation does not forcibly terminate clients that ignore window
/// closure.
///
/// # Errors
/// Returns an error if startup, socket I/O or compositor dispatch fails.
pub async fn serve_scaled(paths: &Paths, scale: UiScale) -> Result<()> {
    actor::serve(paths, scale).await
}

fn notify(state: &[sd_notify::NotifyState<'_>]) {
    if let Err(error) = sd_notify::notify(state) {
        tracing::warn!(%error, "Cannot notify service manager");
    }
}

impl Server {
    fn incoming(&mut self, message: Incoming) {
        match message {
            Incoming::Control(request, reply) => self.start_control(request, reply),
            Incoming::Attach(id, hello, pane) => {
                if self.shutdown.running() && self.connections.contains_key(&id) {
                    self.panes.insert(id, pane);
                    self.send_to_pane(id, ServerToPane::HelloOk);
                    self.send_command(Command::Attach {
                        pane: id,
                        show: hello.show,
                        width: hello.width,
                        height: hello.height,
                    });
                }
            }
            Incoming::PaneMessage(pane, action) => {
                if self.shutdown.running()
                    && self.panes.get(&pane).is_some_and(PaneTransport::active)
                {
                    self.send_command(Command::Pane { pane, action });
                }
            }
        }
    }

    fn event(&mut self, Event { pane, message }: Event) {
        if let ServerToPane::Release(reason) = message {
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.release(reason);
            }
        } else {
            self.send_to_pane(pane, message);
        }
    }

    fn send_to_pane(&mut self, id: PaneId, message: ServerToPane) {
        if self
            .panes
            .get(&id)
            .is_some_and(|pane| pane.active() && !pane.send(message))
        {
            tracing::warn!(?id, "pane output exceeded its byte budget or disconnected");
            if let Some(task) = self.connections.remove(&id) {
                task.abort();
            }
            self.pane_gone(id);
        }
    }

    fn send_command(&mut self, command: Command) {
        if let Err(error) = self.compositor.command(command) {
            tracing::error!(%error, "compositor command queue unavailable");
            self.begin_shutdown();
        }
    }

    fn pane_gone(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_some() {
            self.send_command(Command::Detach { pane: id });
        }
    }

    fn connection_finished(&mut self, id: PaneId, result: io::Result<()>) {
        self.connections.remove(&id);
        self.pane_gone(id);
        if let Err(error) = result {
            tracing::debug!(?id, %error, "connection ended");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.controls.abort_all();
        self.tasks.abort_all();
        self.children.abort_all();
    }
}
