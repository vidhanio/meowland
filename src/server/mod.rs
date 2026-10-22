use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    io,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, LoopHandle, PostAction, RegistrationToken, channel,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
mod lifecycle;
mod process;
mod transport;

use lifecycle::Shutdown;
use process::{Termination, launch_client};
use transport::{AcceptLoop, Pane, accept_control, accept_panes, listeners};

use crate::{
    Error, Result,
    compositor::{self, Command as CompositorCommand, Event as CompositorEvent, PaneId},
    pixels::FrameSize,
    protocol::{
        self, ControlRequest, ControlResponse, Hello, KEY_Q, KEY_W, PaneToServer, ServerToPane,
        WindowInfo,
    },
    signals,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

type IncomingSender = channel::Sender<Incoming>;

#[derive(Clone, Debug)]
pub struct Paths {
    pub control: PathBuf,
    pub pane: PathBuf,
}

impl Paths {
    /// The sockets inside `XDG_RUNTIME_DIR`.
    ///
    /// # Errors
    /// Returns an error when `XDG_RUNTIME_DIR` is unset or is not a directory.
    pub fn discover() -> Result<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::RuntimeDirectoryUnset)?;
        if !runtime.is_dir() {
            return Err(Error::RuntimeDirectoryInvalid(runtime));
        }
        Ok(Self {
            control: runtime.join("meowland-control.sock"),
            pane: runtime.join("meowland-pane.sock"),
        })
    }
}

enum Incoming {
    Control(ControlRequest, Reply),
    Pane(Hello, UnixStream),
    PaneMessage(PaneId, PaneToServer),
    PaneGone(PaneId),
    PaneWriterDone(PaneId),
}

struct Reply {
    sender: Sender<ControlResponse>,
    waiting: Arc<AtomicBool>,
}

impl Reply {
    fn waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }

    fn send(self, response: ControlResponse) {
        let _ = self.sender.send(response);
    }
}

struct Launch {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
}

enum Operation {
    Launch(Launch),
    Close,
}

struct PendingControl {
    operation: Operation,
    reply: Reply,
    deadline: Instant,
    timer: RegistrationToken,
}

struct Server {
    incoming: IncomingSender,
    handle: LoopHandle<'static, Self>,
    commands: channel::Sender<CompositorCommand>,
    display: String,
    panes: HashMap<PaneId, Pane>,
    releasing_panes: HashMap<PaneId, Pane>,
    windows: HashMap<u64, WindowInfo>,
    children: Vec<Child>,
    child_groups: HashSet<u32>,
    pending: HashMap<u64, PendingControl>,
    next_request: u64,
    next_pane: u64,
    listeners: Option<(AcceptLoop, AcceptLoop)>,
    shutdown: Shutdown,
    compositor_alive: bool,
    compositor_failed: bool,
    interrupted: signals::TerminationFlag,
}

/// Run the server using readiness sources and explicit operation deadlines.
///
/// # Errors
/// Fails if startup or event dispatch cannot complete, or the compositor exits
/// unexpectedly.
pub fn serve(paths: &Paths) -> Result<()> {
    let (control, pane) = listeners(paths)?;
    let interrupted = signals::server_termination_flag()?;
    let wakeup = signals::Wakeup::new(&[
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGCHLD,
    ])?;
    let mut event_loop = EventLoop::<Server>::try_new().map_err(io::Error::other)?;
    let handle = event_loop.handle();
    handle
        .insert_source(
            Generic::new(wakeup, calloop::Interest::READ, calloop::Mode::Level),
            |_, source, server| {
                source.drain();
                server.reap_children();
                if server.interrupted.interrupted() {
                    server.begin_shutdown();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    notify(&[sd_notify::NotifyState::Status("Starting compositor")]);
    let (commands, events, display, compositor_thread) = compositor::spawn()?;
    let (incoming, incoming_rx) = channel::channel();
    let setup = register_channels(&handle, incoming_rx, events);
    if let Err(error) = setup {
        let _ = commands.send(CompositorCommand::Shutdown);
        let _ = compositor_thread.join();
        return Err(error.into());
    }
    let listeners = (
        accept_control(control, incoming.clone()),
        accept_panes(pane, incoming.clone()),
    );
    let mut server = Server {
        incoming,
        handle,
        commands,
        display,
        panes: HashMap::new(),
        releasing_panes: HashMap::new(),
        windows: HashMap::new(),
        children: Vec::new(),
        child_groups: HashSet::new(),
        pending: HashMap::new(),
        next_request: 1,
        next_pane: 1,
        listeners: Some(listeners),
        shutdown: Shutdown::Running,
        compositor_alive: true,
        compositor_failed: false,
        interrupted,
    };
    notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Accepting control and pane connections"),
    ]);
    if server.interrupted.interrupted() {
        server.begin_shutdown();
    }
    let result = loop {
        if server.shutdown.stopped() {
            break Ok(());
        }
        if let Err(error) = event_loop.dispatch(None, &mut server) {
            break Err(io::Error::other(error));
        }
    };
    if result.is_err() {
        server.begin_shutdown();
        server.force_shutdown();
    }
    server.listeners.take();
    server.panes.clear();
    server.releasing_panes.clear();
    let _ = compositor_thread.join();
    server.reap_children();
    result?;
    if server.compositor_failed {
        return Err(Error::io(
            "compositor thread stopped unexpectedly",
            io::Error::other("display unavailable"),
        ));
    }
    Ok(())
}

fn register_channels(
    handle: &LoopHandle<'static, Server>,
    incoming: channel::Channel<Incoming>,
    events: channel::Channel<CompositorEvent>,
) -> io::Result<()> {
    handle
        .insert_source(incoming, |event, (), server| {
            if let channel::Event::Msg(message) = event {
                server.incoming(message);
            }
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    handle
        .insert_source(events, |event, (), server| match event {
            channel::Event::Msg(event) => server.compositor_event(event),
            channel::Event::Closed => server.compositor_gone(),
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
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
            Incoming::Pane(hello, socket) => {
                if self.shutdown.running() {
                    self.attach_pane(hello, socket);
                }
            }
            Incoming::PaneMessage(id, message) => {
                if self.panes.contains_key(&id) && self.shutdown.running() {
                    self.pane_message(id, message);
                }
            }
            Incoming::PaneGone(id) => self.pane_gone(id),
            Incoming::PaneWriterDone(id) => {
                self.releasing_panes.remove(&id);
                if self.panes.remove(&id).is_some() {
                    let _ = self.commands.send(CompositorCommand::Detach { pane: id });
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
                self.to_pane(pane, ServerToPane::Title(title));
            }
            CompositorEvent::Cursor { pane, shape } => {
                self.to_pane(pane, ServerToPane::Cursor(shape));
            }
            CompositorEvent::ClipboardWrite { pane, data } => {
                self.to_pane(pane, ServerToPane::ClipboardWrite(data));
            }
            CompositorEvent::ClipboardRead {
                pane,
                request,
                offer,
                mime,
                deadline,
            } => self.to_pane(
                pane,
                ServerToPane::ClipboardRead {
                    request,
                    offer,
                    mime,
                    deadline,
                },
            ),
            CompositorEvent::ClipboardCancel { pane, request } => {
                self.to_pane(pane, ServerToPane::ClipboardCancel { request });
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
            } => self.to_pane(
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

    fn to_pane(&self, pane: PaneId, message: ServerToPane) {
        if let Some(target) = self.panes.get(&pane) {
            target.send(message);
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
                let waiting = reply.waiting.clone();
                let Some(request) = self.begin_control(Operation::Close, reply) else {
                    return;
                };
                CompositorCommand::Kill {
                    request,
                    show,
                    waiting,
                    deadline: self.pending[&request].deadline,
                }
            }
            ControlRequest::Run { args, env, cwd } => {
                let Some(request) =
                    self.begin_control(Operation::Launch(Launch { args, env, cwd }), reply)
                else {
                    return;
                };
                CompositorCommand::CreateActivationToken { request }
            }
        };
        if self.commands.send(command).is_err() {
            self.compositor_failed = true;
            self.begin_shutdown();
        }
    }

    fn begin_control(&mut self, operation: Operation, reply: Reply) -> Option<u64> {
        let request = self.next_request;
        self.next_request += 1;
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let timer =
            match self
                .handle
                .insert_source(Timer::from_deadline(deadline), move |_, (), server| {
                    server.timeout_control(request);
                    TimeoutAction::Drop
                }) {
                Ok(timer) => timer,
                Err(error) => {
                    reply.send(ControlResponse::Error(format!(
                        "scheduling control request: {error}"
                    )));
                    return None;
                }
            };
        self.pending.insert(
            request,
            PendingControl {
                operation,
                reply,
                deadline,
                timer,
            },
        );
        Some(request)
    }

    fn finish_control(&mut self, request: u64) -> Option<PendingControl> {
        let pending = self.pending.remove(&request)?;
        self.handle.remove(pending.timer);
        if pending.reply.waiting() && Instant::now() < pending.deadline && self.shutdown.running() {
            Some(pending)
        } else {
            pending.reply.waiting.store(false, Ordering::Relaxed);
            pending
                .reply
                .send(ControlResponse::Error("control request expired".into()));
            None
        }
    }

    fn timeout_control(&mut self, request: u64) {
        if let Some(pending) = self.pending.remove(&request) {
            pending.reply.waiting.store(false, Ordering::Relaxed);
            pending.reply.send(ControlResponse::Error(
                "compositor request timed out".into(),
            ));
        }
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
                self.child_groups.insert(child.id());
                self.children.push(child);
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

    fn attach_pane(&mut self, hello: Hello, socket: UnixStream) {
        let Ok(writer) = socket.try_clone() else {
            return;
        };
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        let pane = match Pane::start(id, socket, writer, self.incoming.clone()) {
            Ok(pane) => pane,
            Err(error) => {
                tracing::warn!(%error, "pane: cannot own connection");
                return;
            }
        };
        pane.send(ServerToPane::HelloOk);
        self.panes.insert(id, pane);
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
                if text.len() > crate::clipboard::MAX_TEXT {
                    return;
                }
                CompositorCommand::Paste { pane: id, text }
            }
            PaneToServer::ClipboardOffer {
                offer,
                mimes,
                paste,
            } => {
                if mimes.len() > crate::clipboard::MAX_TYPES {
                    return;
                }
                CompositorCommand::ClipboardOffer {
                    pane: id,
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
        if let Some(mut pane) = self.panes.remove(&id) {
            pane.release(reason);
            self.releasing_panes.insert(id, pane);
        }
    }

    fn pane_gone(&mut self, id: PaneId) {
        self.panes.remove(&id);
        let _ = self.commands.send(CompositorCommand::Detach { pane: id });
    }

    fn reap_children(&mut self) {
        self.children.retain_mut(|child| {
            let running = matches!(child.try_wait(), Ok(None));
            if !running {
                self.child_groups.remove(&child.id());
            }
            running
        });
    }
}
