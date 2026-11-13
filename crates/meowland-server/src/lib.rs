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
use meowland_compositor::{Command, Compositor, Event, Runtime};
use meowland_panes::PaneId;
use meowland_protocol::{
    ControlRequest, ControlResponse, Hello, PaneAction, Paths, ServerToPane, Shortcuts,
};
use meowland_settings::Settings;

mod actor;
mod control;
mod error;
mod lifecycle;
mod process;
mod sockets;
mod transport;

pub use error::{Error, Result};
use lifecycle::Shutdown;
use transport::{ConnectionKind, PaneTransport, Sent};

/// One socket budget, with the last slot reserved for control traffic.
const MAX_CONNECTIONS: usize = 256;

type IncomingSender = mpsc::Sender<Incoming>;

/// Identity of one accepted socket, minted before it proves it carries a pane.
///
/// A pane is identified by the connection that carries it, so the two IDs
/// convert into each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ConnectionId(u64);

impl From<ConnectionId> for PaneId {
    fn from(connection: ConnectionId) -> Self {
        Self::from(connection.0)
    }
}

impl From<PaneId> for ConnectionId {
    fn from(pane: PaneId) -> Self {
        Self(pane.get())
    }
}

/// What an accepted connection carries.
enum Role {
    /// A control conversation, or a pane that has not been admitted yet.
    Pending,
    /// An admitted pane and its ordered output.
    Pane(PaneTransport),
}

struct Connection {
    task: AbortHandle,
    role: Role,
}

impl Connection {
    fn release(&mut self, reason: &str) {
        if let Role::Pane(pane) = &mut self.role {
            pane.release(reason.to_owned());
        }
    }
}

/// Outcome of queueing a message for a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    Queued,
    /// No admitted pane with that ID, or the pane was released.
    Unavailable,
    /// The pane exceeded its output budget or its writer is gone; it was
    /// disconnected and its window asked to close.
    Disconnected,
}

enum Incoming {
    Control(ControlRequest, oneshot::Sender<ControlResponse>),
    Focus(PaneId, Hello, PaneTransport),
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
    connections: HashMap<ConnectionId, Connection>,
    next_connection: u64,
    shortcuts: Shortcuts,
    children: Vec<Child>,
    shutdown: Shutdown,
    error: Option<Error>,
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
            Incoming::Focus(id, hello, mut pane) => {
                let running = self.shutdown.running();
                let Some(connection) = self
                    .connections
                    .get_mut(&ConnectionId::from(id))
                    .filter(|connection| matches!(connection.role, Role::Pending))
                else {
                    pane.release("duplicate pane hello".to_owned());
                    return;
                };
                if !running {
                    pane.release("server stopped".to_owned());
                    return;
                }
                connection.role = Role::Pane(pane);
                let shortcuts = self.shortcuts;
                if self.send_to_pane(id, ServerToPane::HelloOk { shortcuts }) != Delivery::Queued {
                    return;
                }
                self.send_command(Command::Focus {
                    pane: id,
                    target: hello.target,
                    follow: hello.follow,
                    size: hello.size,
                });
            }
            Incoming::PaneMessage(pane, action) => {
                let active = self.connections.get(&ConnectionId::from(pane)).is_some_and(
                    |connection| matches!(&connection.role, Role::Pane(pane) if pane.active()),
                );
                if self.shutdown.running() && active {
                    self.send_command(Command::Pane { pane, action });
                }
            }
        }
    }

    fn event(&mut self, Event { pane, message }: Event) {
        if let ServerToPane::Release(reason) = message {
            if let Some(connection) = self.connections.get_mut(&ConnectionId::from(pane)) {
                connection.release(&reason);
            }
        } else {
            self.send_to_pane(pane, message);
        }
    }

    /// Queue a message for an admitted pane, disconnecting a pane whose output
    /// cannot be queued.
    fn send_to_pane(&mut self, id: PaneId, message: ServerToPane) -> Delivery {
        let Some(Connection {
            role: Role::Pane(pane),
            ..
        }) = self.connections.get(&ConnectionId::from(id))
        else {
            return Delivery::Unavailable;
        };
        match pane.send(message) {
            Sent::Queued => Delivery::Queued,
            Sent::Released => Delivery::Unavailable,
            Sent::Rejected => {
                tracing::warn!(?id, "pane output exceeded its byte budget or disconnected");
                if let Some(connection) = self.connections.remove(&ConnectionId::from(id)) {
                    connection.task.abort();
                }
                self.send_command(Command::Close { pane: id });
                Delivery::Disconnected
            }
        }
    }

    fn send_command(&mut self, command: Command) {
        self.compositor.handle_command(command);
    }

    fn connection_finished(&mut self, id: ConnectionId, result: io::Result<()>) {
        if let Some(Connection {
            role: Role::Pane(_),
            ..
        }) = self.connections.remove(&id)
        {
            self.send_command(Command::Close { pane: id.into() });
        }
        if let Err(error) = result {
            tracing::debug!(?id, %error, "connection ended");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for connection in self.connections.values() {
            connection.task.abort();
        }
        if let Some((token, executor)) = self.executor.take() {
            self.handle.remove(token);
            drop(executor);
        }
    }
}
