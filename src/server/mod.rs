use std::{
    collections::{HashMap, HashSet},
    env, io,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::Duration,
};

mod process;
mod transport;

use process::{Xwayland, launch_client, start_xwayland, terminate_tree, xwayland_program};
use transport::{Pane, accept_control, accept_panes, bind};

use crate::{
    Error, Result,
    compositor::{self, Command as CompositorCommand, Event as CompositorEvent},
    pixels::FrameSize,
    protocol::{
        self, ControlRequest, ControlResponse, Hello, KEY_Q, KEY_W, PaneToServer, ServerToPane,
        WindowInfo,
    },
    signals,
};

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
    PaneMessage(u64, PaneToServer),
    PaneGone(u64),
    Compositor(CompositorEvent),
    CompositorGone,
}

/// Timed-out clients must not have their queued requests executed.
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

struct Server {
    incoming: Sender<Incoming>,
    commands: calloop::channel::Sender<CompositorCommand>,
    display: String,
    xwayland: Option<Xwayland>,
    panes: HashMap<u64, Pane>,
    releasing_panes: Vec<Pane>,
    windows: HashMap<u64, WindowInfo>,
    children: Vec<Child>,
    child_groups: HashSet<u32>,
    next_pane: u64,
}

/// Run until a stop request or termination signal.
///
/// # Errors
/// Fails if startup cannot bind sockets, launch the compositor, or install
/// signal handlers.
pub fn serve(paths: &Paths) -> Result<()> {
    let interrupted = signals::termination_flag()?;
    // Binding below still decides; the probe keeps a second server from
    // starting Xwayland only to be turned away.
    if UnixStream::connect(&paths.control).is_ok() {
        return Err(Error::ServerAlreadyListening(paths.control.clone()));
    }
    let (commands, events, display, compositor_thread) = compositor::spawn()?;
    // Bind sockets only after Xwayland startup, so connecting clients can be
    // served immediately.
    let xwayland = start_xwayland_if_configured(&display);
    let (control, pane) = match bind(&paths.control)
        .and_then(|control| bind(&paths.pane).map(|pane| (control, pane)))
    {
        Ok(sockets) => sockets,
        Err(error) => {
            abandon(xwayland, commands, compositor_thread);
            return Err(error);
        }
    };
    let (incoming, incoming_rx) = mpsc::channel();
    let control = accept_control(control, incoming.clone());
    let pane = accept_panes(pane, incoming.clone());
    let compositor_events = thread::spawn({
        let incoming = incoming.clone();
        move || {
            for event in events {
                if incoming.send(Incoming::Compositor(event)).is_err() {
                    return;
                }
            }
            let _ = incoming.send(Incoming::CompositorGone);
        }
    });
    let mut server = Server {
        xwayland,
        incoming,
        commands,
        display,
        panes: HashMap::new(),
        releasing_panes: Vec::new(),
        windows: HashMap::new(),
        children: Vec::new(),
        child_groups: HashSet::new(),
        next_pane: 1,
    };

    let mut compositor_failed = false;
    while !interrupted.interrupted() {
        match incoming_rx.recv_timeout(Duration::from_millis(100)) {
            // A client that has given up must not have its request acted on.
            Ok(Incoming::Control(request, reply)) => {
                if reply.waiting() {
                    let response = server.control(request);
                    reply.send(response);
                }
            }
            Ok(Incoming::Pane(hello, socket)) => server.attach_pane(hello, socket),
            Ok(Incoming::PaneMessage(id, message)) => server.pane_message(id, message),
            Ok(Incoming::PaneGone(id)) => server.pane_gone(id),
            Ok(Incoming::Compositor(event)) => server.compositor_event(event),
            Ok(Incoming::CompositorGone) => {
                compositor_failed = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Do not keep accepting clients into a display whose thread panicked.
        if compositor_thread.is_finished() {
            compositor_failed = true;
            break;
        }
        server.reap_children();
        server.releasing_panes.retain(|pane| !pane.finished());
    }

    // Pending requests and handshakes must not become new clients during
    // shutdown. Dropping replies also wakes waiting control workers.
    drop(incoming_rx);
    drop(control);
    drop(pane);
    server.shutdown(compositor_thread);
    let _ = compositor_events.join();
    if compositor_failed {
        return Err(Error::io(
            "compositor thread stopped unexpectedly",
            io::Error::other("display unavailable"),
        ));
    }
    Ok(())
}

/// Release resources from a server that failed to start.
fn abandon(
    xwayland: Option<Xwayland>,
    commands: calloop::channel::Sender<CompositorCommand>,
    compositor_thread: thread::JoinHandle<()>,
) {
    if let Some(mut xwayland) = xwayland {
        terminate_tree(&HashSet::from([xwayland.child.id()]));
        let _ = xwayland.child.wait();
    }
    // Dropping the last sender is what tells the compositor thread to leave.
    drop(commands);
    let _ = compositor_thread.join();
}

/// X11 startup is optional; report failure without stopping the server.
fn start_xwayland_if_configured(wayland: &str) -> Option<Xwayland> {
    let program = xwayland_program()?;
    match start_xwayland(&program, wayland) {
        Ok(xwayland) => {
            tracing::info!(display = %xwayland.display, program = %program.display(), "xwayland started");
            Some(xwayland)
        }
        Err(error) => {
            tracing::warn!(%error, "xwayland could not start");
            None
        }
    }
}

impl Server {
    fn compositor_event(&mut self, event: CompositorEvent) {
        match event {
            CompositorEvent::WindowUp(info) => {
                self.windows.insert(info.id, info);
            }
            CompositorEvent::WindowDown(id) => {
                self.windows.remove(&id);
            }
            CompositorEvent::Title { pane, title } => {
                self.to_pane(pane, ServerToPane::Title(title));
            }
            CompositorEvent::Cursor { pane, shape } => {
                self.to_pane(pane, ServerToPane::Cursor(shape));
            }
            CompositorEvent::Clipboard { pane, text } => {
                self.to_pane(pane, ServerToPane::Clipboard(text));
            }
            CompositorEvent::Focus(id) => {
                for window in self.windows.values_mut() {
                    window.active = window.id == id;
                }
            }
            CompositorEvent::Frame {
                pane,
                width,
                height,
                y,
                rgb,
            } => {
                self.hand_over_frame(pane, width, height, y, rgb);
            }
            CompositorEvent::Release { pane, reason } => self.release_pane(pane, reason),
        }
    }

    /// The compositor owns presentation backpressure: it sends no new frame
    /// until the pane has acknowledged its previous one. The server only
    /// forwards frames; a second independent queue would retain stale pixels.
    fn hand_over_frame(&self, pane: u64, width: u32, height: u32, y: u32, rgb: Vec<u8>) {
        self.to_pane(
            pane,
            ServerToPane::Frame {
                width,
                height,
                y,
                rgb,
            },
        );
    }

    fn to_pane(&self, pane: u64, message: ServerToPane) {
        if let Some(target) = self.panes.get(&pane) {
            target.send(message);
        }
    }

    fn control(&mut self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::Ping => ControlResponse::Ok,
            ControlRequest::List => {
                let mut list: Vec<_> = self.windows.values().cloned().collect();
                list.sort_by_key(|window| window.id);
                ControlResponse::Windows(list)
            }
            ControlRequest::Run(args) => {
                // `DISPLAY` is only passed on when a satellite is really up,
                // or an inherited one would send the client elsewhere.
                let x11 = self
                    .xwayland
                    .as_ref()
                    .map(|xwayland| xwayland.display.as_str());
                match launch_client(&args, &self.display, x11) {
                    Ok(child) => {
                        self.child_groups.insert(child.id());
                        self.children.push(child);
                        ControlResponse::Ok
                    }
                    Err(error) => ControlResponse::Error(format!("{error:#}")),
                }
            }
        }
    }

    /// Reject unusable panes instead of leaving them to time out.
    fn attach_pane(&mut self, hello: Hello, socket: UnixStream) {
        // Descriptor exhaustion must reject this pane, not end the server.
        let Ok(mut writer) = socket.try_clone() else {
            tracing::warn!("pane: cannot clone socket; dropping connection");
            return;
        };
        if hello.version != protocol::VERSION {
            let _ = protocol::send(
                &mut writer,
                &ServerToPane::Reject(format!(
                    "pane protocol version {} is unsupported; server uses {}",
                    hello.version,
                    protocol::VERSION
                )),
            );
            return;
        }
        if FrameSize::new(hello.width, hello.height).is_none() {
            let _ = protocol::send(
                &mut writer,
                &ServerToPane::Reject("invalid pane dimensions".into()),
            );
            return;
        }
        let id = self.next_pane;
        self.next_pane += 1;
        if protocol::send(&mut writer, &ServerToPane::HelloOk).is_err() {
            return;
        }
        let pane = match Pane::start(id, socket, writer, self.incoming.clone()) {
            Ok(pane) => pane,
            Err(error) => {
                tracing::warn!(%error, "pane: cannot own connection; dropping pane");
                return;
            }
        };
        self.panes.insert(id, pane);
        let _ = self.commands.send(CompositorCommand::Attach {
            pane: id,
            show: hello.show,
            width: hello.width,
            height: hello.height,
        });
    }

    fn pane_message(&mut self, id: u64, message: PaneToServer) {
        match message {
            PaneToServer::Hello(_) => {}
            PaneToServer::Paste(text) => {
                if self.panes.contains_key(&id) && text.len() <= crate::clipboard::MAX_TEXT {
                    let _ = self
                        .commands
                        .send(CompositorCommand::Paste { pane: id, text });
                }
            }
            PaneToServer::Input(event) => match &event {
                protocol::Input::Key {
                    code: KEY_W,
                    pressed: true,
                    modifiers,
                } if protocol::modifiers::alt_only(*modifiers) => {
                    self.release_pane(id, "detached".into());
                    let _ = self.commands.send(CompositorCommand::Detach { pane: id });
                }
                protocol::Input::Key {
                    code: KEY_Q,
                    pressed: true,
                    modifiers,
                } if protocol::modifiers::alt_only(*modifiers) => {
                    let _ = self
                        .commands
                        .send(CompositorCommand::CloseShown { pane: id });
                }
                protocol::Input::Key {
                    code: KEY_Q | KEY_W,
                    pressed: false,
                    modifiers,
                } if protocol::modifiers::alt_only(*modifiers) => {}
                _ => {
                    let _ = self
                        .commands
                        .send(CompositorCommand::Input { pane: id, event });
                }
            },
            PaneToServer::Resize { width, height } => {
                if FrameSize::new(width, height).is_some() {
                    let _ = self.commands.send(CompositorCommand::Resize {
                        pane: id,
                        width,
                        height,
                    });
                }
            }
            PaneToServer::Ack { drawn } => {
                if self.panes.contains_key(&id) {
                    let _ = self
                        .commands
                        .send(CompositorCommand::Ack { pane: id, drawn });
                }
            }
        }
    }

    /// Keep the connection owned while its writer drains the ordered Release.
    fn release_pane(&mut self, id: u64, reason: String) {
        if let Some(mut pane) = self.panes.remove(&id) {
            pane.release(reason);
            self.releasing_panes.push(pane);
        }
    }

    fn pane_gone(&mut self, id: u64) {
        self.panes.remove(&id);
        let _ = self.commands.send(CompositorCommand::Detach { pane: id });
    }

    fn reap_children(&mut self) {
        // A child that is gone takes its pid with it: the raw number is only
        // safe to signal while the process is known to be alive, since a
        // reused pid would put SIGKILL on an unrelated process at shutdown.
        let child_groups = &mut self.child_groups;
        self.children.retain_mut(|child| {
            // A child that cannot be waited for must not remain in the process
            // group set.
            let running = matches!(child.try_wait(), Ok(None));
            if !running {
                child_groups.remove(&child.id());
            }
            running
        });
        if self
            .xwayland
            .as_mut()
            .is_some_and(|running| matches!(running.child.try_wait(), Ok(Some(_))))
        {
            tracing::warn!("xwayland server exited");
            self.xwayland = None;
        }
    }

    /// Listener owners have already stopped accepting before this grace period.
    fn shutdown(&mut self, compositor_thread: thread::JoinHandle<()>) {
        let _ = self.commands.send(CompositorCommand::CloseAll);
        for pane in self.panes.values_mut() {
            pane.release("server stopped".into());
        }
        thread::sleep(Duration::from_millis(250));
        // Unlike normal detach, final shutdown cannot wait for a stalled pane
        // forever. Close both halves and join every established transport.
        self.panes.clear();
        self.releasing_panes.clear();
        if let Some(mut xwayland) = self.xwayland.take() {
            terminate_tree(&HashSet::from([xwayland.child.id()]));
            let _ = xwayland.child.wait();
        }
        terminate_tree(&self.child_groups);
        let _ = self.commands.send(CompositorCommand::Shutdown);
        let _ = compositor_thread.join();
    }
}
