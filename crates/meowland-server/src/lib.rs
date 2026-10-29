//! One calloop owner for Wayland, sockets, application processes, and shutdown.

use std::{collections::HashMap, io, process::Child};

use calloop::{
    Dispatcher, LoopHandle, RegistrationToken,
    futures::{Executor, Scheduler},
};
use futures::{
    channel::{mpsc, oneshot},
    future::AbortHandle,
};
use meowland_compositor::{Command, Compositor, Event, PaneId, Runtime, Settings};
use meowland_core::Result;
use meowland_protocol::{ControlRequest, ControlResponse, Hello, PaneAction, Paths, ServerToPane};

mod actor;
mod control;
mod lifecycle;
mod process;
mod sockets;
mod transport;

use lifecycle::Shutdown;
use transport::{ConnectionKind, PaneTransport};

/// One socket budget, with the last slot reserved for control traffic.
const MAX_CONNECTIONS: usize = 256;

type IncomingSender = mpsc::Sender<Incoming>;

/// Identity of one accepted socket, minted before it proves it carries a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ConnectionId(u64);

impl From<ConnectionId> for PaneId {
    fn from(connection: ConnectionId) -> Self {
        Self(connection.0)
    }
}

impl From<PaneId> for ConnectionId {
    fn from(pane: PaneId) -> Self {
        Self(pane.0)
    }
}

enum Incoming {
    Control(ControlRequest, oneshot::Sender<ControlResponse>),
    Attach(PaneId, Hello, PaneTransport),
    PaneMessage(PaneId, PaneAction),
}

enum Completion {
    Incoming(Option<Incoming>, mpsc::Receiver<Incoming>),
    Connection(ConnectionId, io::Result<()>),
}

struct Listener {
    token: RegistrationToken,
    kind: ConnectionKind,
    enabled: bool,
}

struct Server {
    compositor: Compositor,
    wayland: Runtime,
    display: String,
    handle: LoopHandle<'static, Self>,
    scheduler: Scheduler<Completion>,
    executor: Option<(
        RegistrationToken,
        Dispatcher<'static, Executor<Completion>, Self>,
    )>,
    incoming: IncomingSender,
    listeners: Vec<Listener>,
    connections: HashMap<ConnectionId, AbortHandle>,
    panes: HashMap<PaneId, PaneTransport>,
    next_connection: u64,
    children: Vec<Child>,
    shutdown: Shutdown,
    error: Option<io::Error>,
}

/// Run the single-owner server with output and keyboard settings.
///
/// Binds the supplied paths unless listeners are provided by socket activation.
/// Shutdown gives clients a bounded Wayland-close grace period. With the
/// systemd service, its cgroup also cleans up remaining client processes;
/// standalone operation does not forcibly terminate clients that ignore window
/// closure.
///
/// # Errors
/// Returns an error if startup, socket I/O, or dispatch fails.
pub fn serve(paths: &Paths, settings: &Settings) -> Result<()> {
    actor::serve(paths, settings)
}

fn notify(state: &[sd_notify::NotifyState<'_>]) {
    if let Err(error) = sd_notify::notify(state) {
        tracing::warn!(%error, "Cannot notify service manager");
    }
}

impl Server {
    fn incoming(&mut self, message: Incoming) {
        match message {
            Incoming::Control(request, reply) => self.control(request, reply),
            Incoming::Attach(id, hello, pane) => {
                if self.shutdown.running() && self.connections.contains_key(&ConnectionId::from(id))
                {
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
            if let Some(task) = self.connections.remove(&ConnectionId::from(id)) {
                task.abort();
            }
            self.pane_gone(id);
        }
    }

    fn send_command(&mut self, command: Command) {
        self.compositor.handle_command(command);
    }

    fn pane_gone(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_some() {
            self.send_command(Command::Detach { pane: id });
        }
    }

    fn connection_finished(&mut self, id: ConnectionId, result: io::Result<()>) {
        self.connections.remove(&id);
        self.pane_gone(id.into());
        if let Err(error) = result {
            tracing::debug!(?id, %error, "connection ended");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in self.connections.values() {
            task.abort();
        }
        if let Some((token, executor)) = self.executor.take() {
            self.handle.remove(token);
            drop(executor);
        }
    }
}
