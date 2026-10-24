use std::{
    collections::HashMap,
    io,
    time::{Duration, Instant},
};

use calloop::channel;
use tokio::{sync::mpsc, task::JoinSet};

mod actor;
mod control;
mod lifecycle;
mod process;
mod transport;

use control::{Launch, Operation, PendingControl, Reply, Requests};
use lifecycle::{CompositorStatus, Shutdown};
use meowland_compositor::{
    self as compositor, Command as CompositorCommand, Event as CompositorEvent, PaneId,
};
use meowland_core::{Result, pixels::FrameSize, signals};
use meowland_protocol::{
    self as protocol, ControlRequest, ControlResponse, KEY_Q, KEY_W, PaneToServer, Paths,
    ServerToPane, WindowInfo,
};
use process::{Children, Termination, launch_client};
use transport::{Listeners, Pane, ReadyPane};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

type IncomingSender = mpsc::Sender<Incoming>;

enum Incoming {
    Control(ControlRequest, Reply),
    PaneMessage(PaneId, PaneToServer),
}

struct Server {
    incoming: IncomingSender,
    messages: mpsc::Receiver<Incoming>,
    events: mpsc::UnboundedReceiver<CompositorEvent>,
    commands: channel::Sender<CompositorCommand>,
    display: String,
    panes: HashMap<PaneId, Pane>,
    windows: HashMap<u64, WindowInfo>,
    children: Children,
    requests: Requests,
    next_pane: u64,
    listeners: Option<Listeners>,
    controls: JoinSet<io::Result<()>>,
    handshakes: JoinSet<io::Result<ReadyPane>>,
    connections: JoinSet<PaneId>,
    accept_retry: Option<Instant>,
    shutdown: Shutdown,
    compositor: CompositorStatus,
    interrupted: signals::TerminationFlag,
}

/// Run the single-owner server actor on Tokio, with a dedicated compositor
/// thread.
///
/// # Errors
/// Fails if startup or socket I/O cannot complete, or the compositor exits
/// unexpectedly.
pub async fn serve(paths: &Paths) -> Result<()> {
    serve_scaled(paths, meowland_core::scale::UiScale::default()).await
}

/// Run the compositor server with a shared UI scale for all terminal panes.
///
/// # Errors
/// Fails if startup or socket I/O cannot complete, or the compositor exits
/// unexpectedly.
pub async fn serve_scaled(paths: &Paths, scale: meowland_core::scale::UiScale) -> Result<()> {
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
            Incoming::Control(request, reply) => {
                if !reply.waiting() {
                    return;
                }
                if self.shutdown.running() {
                    self.control(request, reply);
                } else {
                    reply.send(ControlResponse::Error("server stopping".into()));
                }
            }
            Incoming::PaneMessage(id, message) => {
                if self.panes.get(&id).is_some_and(Pane::active) && self.shutdown.running() {
                    self.pane_message(id, message);
                }
            }
        }
    }

    fn compositor_event(&mut self, event: CompositorEvent) {
        match event {
            CompositorEvent::ActivationCreated { request, token } => {
                self.activation_created(request, token);
            }
            CompositorEvent::CloseRequested { request, result } => {
                if let Some(pending) = self.finish_control(request) {
                    pending
                        .reply
                        .send(result.map_or_else(ControlResponse::Error, |()| ControlResponse::Ok));
                }
            }
            CompositorEvent::WindowUp(info) => {
                self.windows.insert(info.id, info);
            }
            CompositorEvent::WindowDown(id) => {
                self.windows.remove(&id.0);
            }
            CompositorEvent::Title { pane, title } => {
                self.send_to_pane(pane, ServerToPane::Title(title));
            }
            CompositorEvent::Cursor { pane, shape } => {
                self.send_to_pane(pane, ServerToPane::Cursor(shape));
            }
            CompositorEvent::Bell { pane } => {
                self.send_to_pane(pane, ServerToPane::Bell);
            }
            CompositorEvent::ClipboardWrite {
                pane,
                selection,
                data,
            } => {
                self.send_to_pane(pane, ServerToPane::ClipboardWrite { selection, data });
            }
            CompositorEvent::ClipboardRead {
                pane,
                request,
                offer,
                mime,
                deadline,
            } => self.send_to_pane(
                pane,
                ServerToPane::ClipboardRead {
                    request,
                    offer,
                    mime,
                    deadline,
                },
            ),
            CompositorEvent::ClipboardCancel { pane, request } => {
                self.send_to_pane(pane, ServerToPane::ClipboardCancel { request });
            }
            CompositorEvent::Focus(id) => {
                for window in self.windows.values_mut() {
                    window.active = Some(window.id) == id.map(|id| id.0);
                }
            }
            CompositorEvent::Frame {
                pane,
                width,
                height,
                y,
                rgb,
            } => self.send_to_pane(
                pane,
                ServerToPane::Frame {
                    width,
                    height,
                    y,
                    rgb,
                },
            ),
            CompositorEvent::Release { pane, reason } => self.release_pane(pane, reason),
        }
    }

    fn send_to_pane(&mut self, pane: PaneId, message: ServerToPane) {
        if self
            .panes
            .get(&pane)
            .is_some_and(|target| target.active() && !target.send(message))
        {
            tracing::warn!(
                ?pane,
                "pane output stalled or disconnected; closing connection"
            );
            self.pane_gone(pane);
        }
    }

    fn control(&mut self, request: ControlRequest, reply: Reply) {
        let command = match request {
            ControlRequest::Ping => {
                reply.send(ControlResponse::Ok);
                return;
            }
            ControlRequest::List => {
                let mut list: Vec<_> = self.windows.values().cloned().collect();
                list.sort_by_key(|window| window.id);
                reply.send(ControlResponse::Windows(list));
                return;
            }
            ControlRequest::Kill(show) => {
                let waiting = reply.waiting_handle();
                let begun = self.requests.begin(Operation::Close, reply);
                CompositorCommand::Kill {
                    request: begun.request,
                    show,
                    waiting,
                    deadline: begun.deadline,
                }
            }
            ControlRequest::Run { args, env, cwd } => {
                let begun = self
                    .requests
                    .begin(Operation::Launch(Launch { args, env, cwd }), reply);
                CompositorCommand::CreateActivationToken {
                    request: begun.request,
                }
            }
        };
        if self.commands.send(command).is_err() {
            self.compositor = CompositorStatus::Failed;
            self.begin_shutdown();
        }
    }

    fn finish_control(&mut self, request: u64) -> Option<PendingControl> {
        self.requests.finish(request, self.shutdown.running())
    }

    fn activation_created(&mut self, request: u64, token: String) {
        let Some(pending) = self.finish_control(request) else {
            let _ = self
                .commands
                .send(CompositorCommand::CancelActivation { token });
            return;
        };
        let Operation::Launch(launch) = pending.operation else {
            let _ = self
                .commands
                .send(CompositorCommand::CancelActivation { token });
            pending.reply.send(ControlResponse::Error(
                "unexpected activation response".into(),
            ));
            return;
        };
        match launch_client(
            &launch.args,
            &launch.env,
            &launch.cwd,
            &self.display,
            &token,
        ) {
            Ok(child) => {
                self.children.add(child);
                pending.reply.send(ControlResponse::Started(token));
            }
            Err(error) => {
                let _ = self
                    .commands
                    .send(CompositorCommand::CancelActivation { token });
                pending
                    .reply
                    .send(ControlResponse::Error(format!("{error:#}")));
            }
        }
    }

    fn attach_pane(&mut self, ReadyPane { hello, socket }: ReadyPane) {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        let pane = Pane::start(id, socket, self.incoming.clone(), &mut self.connections);
        self.panes.insert(id, pane);
        self.send_to_pane(id, ServerToPane::HelloOk);
        let _ = self.commands.send(CompositorCommand::Attach {
            pane: id,
            show: hello.show,
            width: hello.width,
            height: hello.height,
        });
    }

    fn pane_message(&mut self, id: PaneId, message: PaneToServer) {
        let command = match message {
            PaneToServer::Hello(_) => return,
            PaneToServer::Paste(text) => {
                if text.len() > meowland_protocol::clipboard::MAX_TEXT {
                    return;
                }
                CompositorCommand::Paste { pane: id, text }
            }
            PaneToServer::ClipboardOffer {
                selection,
                offer,
                mimes,
                paste,
            } => {
                if mimes.len() > meowland_protocol::clipboard::MAX_TYPES {
                    return;
                }
                CompositorCommand::ClipboardOffer {
                    pane: id,
                    selection,
                    offer,
                    mimes,
                    paste,
                }
            }
            PaneToServer::ClipboardReply { request, data } => CompositorCommand::ClipboardReply {
                pane: id,
                request,
                data,
            },
            PaneToServer::Input(event) => match &event {
                protocol::Input::Key {
                    code: KEY_W,
                    pressed: true,
                    modifiers,
                    ..
                } if protocol::modifiers::alt_only(*modifiers) => {
                    self.release_pane(id, "detached".into());
                    CompositorCommand::Detach { pane: id }
                }
                protocol::Input::Key {
                    code: KEY_Q,
                    pressed: true,
                    modifiers,
                    ..
                } if protocol::modifiers::alt_only(*modifiers) => {
                    CompositorCommand::CloseShown { pane: id }
                }
                _ => CompositorCommand::Input { pane: id, event },
            },
            PaneToServer::Resize { width, height } => {
                if FrameSize::new(width, height).is_none() {
                    return;
                }
                CompositorCommand::Resize {
                    pane: id,
                    width,
                    height,
                }
            }
            PaneToServer::Ack { drawn } => CompositorCommand::Ack { pane: id, drawn },
        };
        let _ = self.commands.send(command);
    }

    fn release_pane(&mut self, id: PaneId, reason: String) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.release(reason);
        }
    }

    fn pane_gone(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_some() {
            let _ = self.commands.send(CompositorCommand::Detach { pane: id });
        }
    }
}
