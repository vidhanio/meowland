//! Socket I/O and process ownership, with rendering isolated on its own thread.

use std::{collections::HashMap, io};

use meowland_compositor::{Command, Event, PaneId};
use meowland_core::{Result, scale::UiScale};
use meowland_protocol::{
    ControlRequest, ControlResponse, Hello, KEY_Q, KEY_W, PaneToServer, Paths, ServerToPane,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinSet},
};

mod actor;
mod compositor;
mod lifecycle;
mod process;
mod sockets;
mod transport;

use compositor::DisplayThread;
use lifecycle::Shutdown;
use process::{Children, launch_client};
use sockets::Listeners;
use transport::Pane;

/// One socket budget, with the last slot reserved for control traffic.
const MAX_CONNECTIONS: usize = 256;

type IncomingSender = mpsc::Sender<Incoming>;

enum Incoming {
    Control(ControlRequest, oneshot::Sender<ControlResponse>),
    Attach(PaneId, Hello, Pane),
    PaneMessage(PaneId, PaneToServer),
}

struct Server {
    compositor: DisplayThread,
    incoming: IncomingSender,
    messages: mpsc::Receiver<Incoming>,
    listeners: Option<Listeners>,
    connections: HashMap<PaneId, AbortHandle>,
    tasks: JoinSet<(PaneId, io::Result<()>)>,
    panes: HashMap<PaneId, Pane>,
    next_connection: u64,
    children: Children,
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
    async fn incoming(&mut self, message: Incoming) {
        match message {
            Incoming::Control(request, mut reply) => {
                if reply.is_closed() {
                    return;
                }
                let response = if self.shutdown.running() {
                    tokio::select! {
                        result = self.control(request) => result.unwrap_or_else(|error| ControlResponse::Error(error.to_string())),
                        () = reply.closed() => return,
                    }
                } else {
                    ControlResponse::Error("server stopping".into())
                };
                let _ = reply.send(response);
            }
            Incoming::Attach(id, hello, pane) => {
                if self.shutdown.running() && self.connections.contains_key(&id) {
                    self.panes.insert(id, pane);
                    self.send_to_pane(id, ServerToPane::HelloOk);
                    let _ = self.compositor.command(Command::Attach {
                        pane: id,
                        show: hello.show,
                        width: hello.width,
                        height: hello.height,
                    });
                }
            }
            Incoming::PaneMessage(id, message) => self.pane_message(id, message),
        }
    }

    async fn control(&mut self, request: ControlRequest) -> io::Result<ControlResponse> {
        Ok(match request {
            ControlRequest::Ping => ControlResponse::Ok,
            ControlRequest::List => ControlResponse::Windows(
                self.compositor
                    .call(|compositor| compositor.window_list())
                    .await?,
            ),
            ControlRequest::Kill(show) => self
                .compositor
                .call(move |compositor| compositor.close_window(&show))
                .await?
                .map_or_else(ControlResponse::Error, |()| ControlResponse::Ok),
            ControlRequest::Run { args, env, cwd } => {
                let token = self
                    .compositor
                    .call(meowland_compositor::Compositor::create_activation_token)
                    .await?;
                match launch_client(&args, &env, &cwd, &self.compositor.display, &token) {
                    Ok(child) => {
                        self.children.add(child);
                        ControlResponse::Started(token)
                    }
                    Err(error) => {
                        self.cancel_activation(token);
                        ControlResponse::Error(format!("{error:#}"))
                    }
                }
            }
        })
    }

    fn cancel_activation(&self, token: String) {
        let _ = self
            .compositor
            .execute(move |compositor| compositor.cancel_activation(&token, "launch cancelled"));
    }

    fn pane_message(&mut self, pane: PaneId, message: PaneToServer) {
        if !self.shutdown.running() || !self.panes.get(&pane).is_some_and(Pane::active) {
            return;
        }
        let command = match message {
            PaneToServer::Hello(_) => return,
            PaneToServer::Paste(text) => Command::Paste { pane, text },
            PaneToServer::ClipboardOffer {
                selection,
                offer,
                mimes,
                paste,
            } => Command::ClipboardOffer {
                pane,
                selection,
                offer,
                mimes,
                paste,
            },
            PaneToServer::ClipboardReply { request, data } => Command::ClipboardReply {
                pane,
                request,
                data,
            },
            PaneToServer::Input(event) => match &event {
                meowland_protocol::Input::Key {
                    code: KEY_W,
                    pressed: true,
                    modifiers,
                    ..
                } if meowland_protocol::modifiers::alt_only(*modifiers) => {
                    if let Some(pane) = self.panes.get_mut(&pane) {
                        pane.release("detached".into());
                    }
                    Command::Detach { pane }
                }
                meowland_protocol::Input::Key {
                    code: KEY_Q,
                    pressed: true,
                    modifiers,
                    ..
                } if meowland_protocol::modifiers::alt_only(*modifiers) => {
                    Command::CloseShown { pane }
                }
                _ => Command::Input { pane, event },
            },
            PaneToServer::Resize { width, height } => Command::Resize {
                pane,
                width,
                height,
            },
            PaneToServer::Ack { drawn } => Command::Ack { pane, drawn },
        };
        let _ = self.compositor.command(command);
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

    fn pane_gone(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_some() {
            let _ = self.compositor.command(Command::Detach { pane: id });
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
        self.tasks.abort_all();
        self.terminate_children();
    }
}
