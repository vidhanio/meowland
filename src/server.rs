use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use crate::{
    compositor::{
        self, Command as CompositorCommand, Event as CompositorEvent, MAX_SURFACE_PIXELS,
        MAX_SURFACE_SIDE,
    },
    diag,
    protocol::{
        self, ControlRequest, ControlResponse, Hello, KEY_Q, KEY_W, PaneToServer, ServerToPane,
        WindowInfo,
    },
};

#[derive(Clone, Debug)]
pub struct Paths {
    pub control: PathBuf,
    pub pane: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    /// The socket and log paths inside `XDG_RUNTIME_DIR`.
    ///
    /// # Errors
    /// Returns an error when `XDG_RUNTIME_DIR` is unset or is not a directory.
    pub fn discover() -> Result<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is not set")?;
        if !runtime.is_dir() {
            bail!("XDG_RUNTIME_DIR is not a directory");
        }
        Ok(Self {
            control: runtime.join("meowland-control.sock"),
            pane: runtime.join("meowland-pane.sock"),
            log: diag::path(&runtime.join("meowland-pane.sock")),
        })
    }
}

enum Incoming {
    Control(ControlRequest, Reply),
    Pane(Hello, UnixStream),
    PaneMessage(u64, PaneToServer),
    PaneGone(u64),
}

/// The way back to the client that asked, and whether it is still waiting.
/// The handler thread stops waiting after its own timeout, and a request
/// nobody is waiting for must not be carried out.
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

struct Pane {
    writer: Sender<ServerToPane>,
    /// A frame is with the pane until it acknowledges one.
    busy: bool,
    /// The newest frame that arrived while the pane was busy.  The compositor
    /// treats a frame it hands over as displayed, so one that is not sent now
    /// would be lost; holding the newest keeps the pane on the last scene it
    /// was shown without queueing frames the next one supersedes.
    held: Option<ServerToPane>,
}

/// Rootless X11 clients, through `xwayland-satellite`.
///
/// Xwayland owns its display number for as long as it runs: it takes the
/// standard lock file and creates `/tmp/.X11-unix/X<n>`, and removes both on
/// exit.  The satellite is told which number to use and the socket is watched
/// to prove the server came up.
struct Xwayland {
    child: Child,
    display: String,
}

/// Everything the server keeps between events: the panes, the window list the
/// control socket reports, and the clients it started.
struct Server<'a> {
    paths: &'a Paths,
    incoming: Sender<Incoming>,
    commands: Sender<CompositorCommand>,
    display: String,
    xwayland: Option<Xwayland>,
    panes: HashMap<u64, Pane>,
    windows: HashMap<u64, WindowInfo>,
    children: Vec<Child>,
    child_groups: HashSet<u32>,
    next_pane: u64,
    stopping: bool,
}

/// Run the server until it is asked to stop or a watched signal arrives.
///
/// # Errors
/// Returns an error when a socket cannot be bound, the compositor cannot be
/// started, or the signal handlers cannot be installed.  Everything the server
/// started itself is a warning, never a failure.
pub fn serve(paths: &Paths) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))?;
    }
    let (commands, events, display, compositor_thread) = compositor::spawn()?;
    // Xwayland is started before either socket exists, because choosing a
    // display can take seconds and a client that can connect is one the loop
    // below can answer: a bound socket that nothing reads yet is a client
    // that times out by construction.
    let xwayland = start_xwayland_if_configured(paths, &display);
    let sockets = || -> Result<(UnixListener, UnixListener)> {
        Ok((bind(&paths.control)?, bind(&paths.pane)?))
    };
    let (control, pane) = match sockets() {
        Ok(sockets) => sockets,
        Err(error) => {
            let _ = fs::remove_file(&paths.control);
            abandon(xwayland, commands, compositor_thread);
            return Err(error);
        }
    };
    let (incoming, incoming_rx) = mpsc::channel();
    accept_control(control, incoming.clone(), paths.log.clone());
    accept_panes(pane, incoming.clone(), paths.log.clone());
    let mut server = Server {
        xwayland,
        paths,
        incoming,
        commands,
        display,
        panes: HashMap::new(),
        windows: HashMap::new(),
        children: Vec::new(),
        child_groups: HashSet::new(),
        next_pane: 1,
        stopping: false,
    };

    while !server.stopping && !interrupted.load(Ordering::Relaxed) {
        server.drain_compositor_events(&events);
        match incoming_rx.recv_timeout(Duration::from_millis(10)) {
            // A client that has given up is not there to wait for the reply,
            // and a request must not still be carried out (starting a client,
            // say) long after it was asked for.
            Ok(Incoming::Control(request, reply)) => {
                if reply.waiting() {
                    let response = server.control(request);
                    reply.send(response);
                }
            }
            Ok(Incoming::Pane(hello, socket)) => server.attach_pane(hello, socket),
            Ok(Incoming::PaneMessage(id, message)) => server.pane_message(id, message),
            Ok(Incoming::PaneGone(id)) => server.pane_gone(id),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        server.reap_children();
    }

    server.shutdown(compositor_thread);
    Ok(())
}

/// Give back everything a half-started server owns: the Xwayland it started,
/// the compositor thread, and the caller's error.
fn abandon(
    xwayland: Option<Xwayland>,
    commands: Sender<CompositorCommand>,
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

/// X11 support is a convenience, never a reason to fail: a satellite that
/// cannot start is logged and forgotten.
fn start_xwayland_if_configured(paths: &Paths, wayland: &str) -> Option<Xwayland> {
    let program = xwayland_program()?;
    match start_xwayland(&program, wayland, &paths.log) {
        Ok(xwayland) => {
            diag::line(
                &paths.log,
                &format!(
                    "xwayland: DISPLAY={} via {}",
                    xwayland.display,
                    program.display()
                ),
            );
            Some(xwayland)
        }
        Err(error) => {
            diag::line(&paths.log, &format!("xwayland: not started: {error:#}"));
            None
        }
    }
}

impl Server<'_> {
    fn drain_compositor_events(&mut self, events: &Receiver<CompositorEvent>) {
        while let Ok(event) = events.try_recv() {
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
                } => self.hand_over_frame(pane, width, height, y, rgb),
                CompositorEvent::Release { pane, reason } => {
                    if let Some(target) = self.panes.remove(&pane) {
                        release(&target.writer, reason);
                    }
                }
            }
        }
    }

    /// A frame goes to a pane that has acknowledged the last one.  One that is
    /// still busy keeps only the newest frame until its ack arrives: the
    /// compositor treats a frame it handed over as displayed, so a frame that
    /// is dropped here would leave the pane on a scene the compositor thinks
    /// it has replaced.
    fn hand_over_frame(&mut self, pane: u64, width: u32, height: u32, y: u32, rgb: Vec<u8>) {
        let frame = ServerToPane::Frame {
            width,
            height,
            y,
            rgb,
        };
        let Some(target) = self.panes.get_mut(&pane) else {
            return;
        };
        if target.busy {
            // Only the newest band is worth holding: the next one is the whole
            // of what a pane that comes back needs to be current.
            target.held = Some(frame);
            return;
        }
        if target.writer.send(frame).is_ok() {
            target.busy = true;
        }
    }

    fn to_pane(&self, pane: u64, message: ServerToPane) {
        if let Some(target) = self.panes.get(&pane) {
            let _ = target.writer.send(message);
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
                    .map(|xwayland| xwayland.display.clone());
                match launch_client(&args, &self.display, x11.as_deref(), &self.paths.log) {
                    Ok(child) => {
                        self.child_groups.insert(child.id());
                        self.children.push(child);
                        ControlResponse::Ok
                    }
                    Err(error) => ControlResponse::Error(format!("{error:#}")),
                }
            }
            ControlRequest::Stop => {
                self.stopping = true;
                ControlResponse::Ok
            }
        }
    }

    /// A pane that has just said hello.  One the server cannot use is told why
    /// rather than left to time out.
    fn attach_pane(&mut self, hello: Hello, socket: UnixStream) {
        // Descriptor exhaustion must reject this pane, not end the server.
        let Ok(mut writer) = socket.try_clone() else {
            diag::line(
                &self.paths.log,
                "pane: cannot clone the socket; dropping it",
            );
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
        if !valid_size(hello.width, hello.height) {
            let _ = protocol::send(
                &mut writer,
                &ServerToPane::Reject("invalid pane dimensions".into()),
            );
            return;
        }
        let id = self.next_pane;
        self.next_pane += 1;
        let (send_tx, send_rx) = mpsc::channel();
        let _ = protocol::send(&mut writer, &ServerToPane::HelloOk);
        let log = self.paths.log.clone();
        thread::spawn(move || pane_writer(writer, send_rx, &log));
        read_pane(id, socket, self.incoming.clone());
        self.panes.insert(
            id,
            Pane {
                writer: send_tx,
                busy: false,
                held: None,
            },
        );
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
            PaneToServer::Input(event) => match &event {
                protocol::Input::Key {
                    code: KEY_W,
                    pressed: true,
                    modifiers,
                } if protocol::modifiers::alt_only(*modifiers) => {
                    if let Some(target) = self.panes.remove(&id) {
                        release(&target.writer, "detached".into());
                    }
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
            PaneToServer::Resize { width, height, .. } => {
                if valid_size(width, height) {
                    let _ = self.commands.send(CompositorCommand::Resize {
                        pane: id,
                        width,
                        height,
                    });
                }
            }
            PaneToServer::Ack { drawn } => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.busy = false;
                    // The frame held back while this one was in flight is the
                    // one the compositor last rendered; it goes out now.
                    if let Some(held) = pane.held.take()
                        && pane.writer.send(held).is_ok()
                    {
                        pane.busy = true;
                    }
                    let _ = self
                        .commands
                        .send(CompositorCommand::Ack { pane: id, drawn });
                }
            }
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
        let mut gone = Vec::new();
        self.children.retain_mut(|child| {
            // A child that is gone, or one that cannot be waited for at all,
            // is not one that will be reaped later.
            let running = matches!(child.try_wait(), Ok(None));
            if !running {
                gone.push(child.id());
            }
            running
        });
        for pid in gone {
            self.child_groups.remove(&pid);
        }
        if self
            .xwayland
            .as_mut()
            .is_some_and(|running| matches!(running.child.try_wait(), Ok(Some(_))))
        {
            diag::line(&self.paths.log, "xwayland: server exited");
            self.xwayland = None;
        }
    }

    /// Let go in the order a pane and a client have to see: ask the windows to
    /// close, release the panes (the terminal is given back on EOF), then
    /// signal whatever is left.
    ///
    /// The socket files go first: they are what says a server is there, and
    /// `server stop` returning while they still resolve is a `run` right after
    /// it that connects to a server already on its way out.
    fn shutdown(&mut self, compositor_thread: thread::JoinHandle<()>) {
        let _ = fs::remove_file(&self.paths.control);
        let _ = fs::remove_file(&self.paths.pane);
        let _ = self.commands.send(CompositorCommand::CloseAll);
        thread::sleep(Duration::from_millis(250));
        self.panes.clear();
        if let Some(mut xwayland) = self.xwayland.take() {
            terminate_tree(&HashSet::from([xwayland.child.id()]));
            let _ = xwayland.child.wait();
        }
        terminate_tree(&self.child_groups);
        let _ = self.commands.send(CompositorCommand::Shutdown);
        let _ = compositor_thread.join();
    }
}

/// Tell a pane why it is being let go.  The pane may still have a frame in
/// flight, so this is queued behind it rather than dropped when the queue
/// happens to be busy.  Frames are paced by the pane's acks, so the queue
/// holds at most one frame plus a few short messages.
fn release(writer: &Sender<ServerToPane>, reason: String) {
    let _ = writer.send(ServerToPane::Release(reason));
}

fn valid_size(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_SURFACE_SIDE
        && height <= MAX_SURFACE_SIDE
        && u64::from(width) * u64::from(height) <= MAX_SURFACE_PIXELS as u64
}

fn bind(path: &Path) -> Result<UnixListener> {
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                bail!("server already listening at {}", path.display());
            }
            fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            UnixListener::bind(path)?
        }
        Err(error) => return Err(error.into()),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// How long the pane has to say hello.  A pane connects as soon as it starts,
/// but it only sends its hello after it has probed the terminal it is in,
/// which it gives up to a second to answer.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the pane handshake writes from the event loop may take.  They are
/// a few bytes on a fresh socket, so this is only a guard against a peer that
/// never reads at all: the loop must not be held up for long.
const HANDSHAKE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one frame may take to reach the pane.  A frame is megabytes and
/// the pane is a process on the same machine: it is read as fast as the
/// terminal takes it, and a busy machine can leave the pane unrun for seconds
/// at a time.  Killing the pane over that would end a session that only had to
/// wait, so the writer is patient; a pane that is really gone closes its
/// socket, which fails the write immediately.
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a listener error is one the accept loop can wait out.  Running out
/// of descriptors is the one that happens on a long-lived server with many
/// panes; the rest are transient at the socket layer.  The listener is the
/// only way into a running server, so its thread must not leave it behind.
fn transient_accept(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .iter()
        .any(|candidate| Some(candidate.raw_os_error()) == error.raw_os_error())
}

fn accept_control(listener: UnixListener, incoming: Sender<Incoming>, log: PathBuf) {
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    let incoming = incoming.clone();
                    thread::spawn(move || {
                        let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
                        let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
                        if let Ok(request) = protocol::recv::<ControlRequest>(&mut socket) {
                            let (reply_tx, reply_rx) = mpsc::channel();
                            let waiting = Arc::new(AtomicBool::new(true));
                            let reply = Reply {
                                sender: reply_tx,
                                waiting: Arc::clone(&waiting),
                            };
                            if incoming.send(Incoming::Control(request, reply)).is_ok()
                                && let Ok(answer) = reply_rx.recv_timeout(Duration::from_secs(2))
                            {
                                let _ = protocol::send(&mut socket, &answer);
                            }
                            // The client gives up when this returns without an
                            // answer, so the request may no longer be acted on.
                            waiting.store(false, Ordering::Relaxed);
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if transient_accept(&error) => {
                    diag::line(&log, &format!("control: accept failed: {error}; retrying"));
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    diag::line(&log, &format!("control: accept stopped: {error}"));
                    break;
                }
            }
        }
    });
}

fn accept_panes(listener: UnixListener, incoming: Sender<Incoming>, log: PathBuf) {
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    let incoming = incoming.clone();
                    let log = log.clone();
                    thread::spawn(move || {
                        let _ = socket.set_read_timeout(Some(HELLO_TIMEOUT));
                        // The hello and any rejection are written from the event
                        // loop, which must not be held up by a peer that stopped
                        // reading.
                        let _ = socket.set_write_timeout(Some(HANDSHAKE_WRITE_TIMEOUT));
                        match protocol::recv::<PaneToServer>(&mut socket) {
                            Ok(PaneToServer::Hello(hello)) => {
                                let _ = socket.set_read_timeout(None);
                                let _ = incoming.send(Incoming::Pane(hello, socket));
                            }
                            // A pane the server cannot use is told why rather than
                            // left to time out.
                            Ok(other) => {
                                diag::line(
                                    &log,
                                    &format!("pane: first message was not a hello: {other:?}"),
                                );
                                let _ = protocol::send(
                                    &mut socket,
                                    &ServerToPane::Reject(
                                        "the pane did not start with a hello".into(),
                                    ),
                                );
                            }
                            Err(error) => {
                                diag::line(&log, &format!("pane: no hello: {error}"));
                                let _ = protocol::send(
                                    &mut socket,
                                    &ServerToPane::Reject(format!(
                                        "the pane hello failed: {error}"
                                    )),
                                );
                            }
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if transient_accept(&error) => {
                    diag::line(&log, &format!("pane: accept failed: {error}; retrying"));
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    diag::line(&log, &format!("pane: accept stopped: {error}"));
                    break;
                }
            }
        }
    });
}

/// Frames are the compositor's, so `commands` is only borrowed: the thread
/// that spawned this one owns the sender for its whole life.
fn pane_writer(mut socket: UnixStream, rx: Receiver<ServerToPane>, log: &Path) {
    let _ = socket.set_write_timeout(Some(FRAME_WRITE_TIMEOUT));
    for message in rx {
        if let Err(error) = protocol::send(&mut socket, &message) {
            // A write that did not finish leaves the stream half a message
            // long, so there is no way back except closing it; the pane's
            // reader sees the end too and the pane is reaped.
            let _ = socket.shutdown(std::net::Shutdown::Both);
            diag::line(log, &format!("pane: write failed ({error}); closed"));
            break;
        }
    }
}

fn read_pane(id: u64, mut socket: UnixStream, incoming: Sender<Incoming>) {
    thread::spawn(move || {
        while let Ok(message) = protocol::recv::<PaneToServer>(&mut socket) {
            if incoming.send(Incoming::PaneMessage(id, message)).is_err() {
                return;
            }
        }
        let _ = incoming.send(Incoming::PaneGone(id));
    });
}

fn launch_client(
    args: &[String],
    wayland: &str,
    x11: Option<&str>,
    log_path: &Path,
) -> Result<Child> {
    let Some(executable) = args.first() else {
        bail!("run requires a command");
    };
    let log = diag::open(log_path)?;
    let mut command = Command::new(executable);
    command.args(&args[1..]).env("WAYLAND_DISPLAY", wayland);
    match x11 {
        Some(display) => {
            command.env("DISPLAY", display);
        }
        None => {
            command.env_remove("DISPLAY");
        }
    }
    command
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "auto")
        .env("XDG_SESSION_TYPE", "wayland")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("starting {executable}"))
}

/// Signal every process in the trees rooted at `roots`, and the process group
/// of each root.
///
/// The tree is rescanned before each signal, because Unix does not take
/// children down with their parent: a helper that forked on the way out would
/// otherwise be missed by every signal.  Escalation is `SIGHUP`, `SIGTERM`,
/// `SIGKILL`, with a moment between them for a process to leave on its own
/// terms.
fn terminate_tree(roots: &HashSet<u32>) {
    for signal in [Signal::HUP, Signal::TERM, Signal::KILL] {
        for raw in descendants(roots).into_iter().chain(roots.iter().copied()) {
            if let Some(pid) = process_id(raw) {
                let _ = kill_process(pid, signal);
            }
        }
        // Every child was started in its own process group, so the group goes
        // too: a daemonised grandchild that left the tree is still in it.
        for raw in roots {
            if let Some(group) = process_id(*raw) {
                let _ = kill_process_group(group, signal);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// `Pid::from_raw` refuses 0 and negatives, so nothing here can signal a group
/// by mistake.
fn process_id(raw: u32) -> Option<Pid> {
    i32::try_from(raw).ok().and_then(Pid::from_raw)
}

fn descendants(roots: &HashSet<u32>) -> HashSet<u32> {
    let mut parents = HashMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(rest) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
                continue;
            };
            if let Some(parent) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u32>().ok())
            {
                parents.insert(pid, parent);
            }
        }
    }
    let mut result = HashSet::new();
    let mut frontier: Vec<u32> = roots.iter().copied().collect();
    while let Some(parent) = frontier.pop() {
        for (&pid, &ppid) in &parents {
            if ppid == parent && result.insert(pid) {
                frontier.push(pid);
            }
        }
    }
    result
}

/// Where `xwayland-satellite` comes from.
///
/// `MEOWLAND_XWAYLAND` may disable it (`off`), force the normal lookup
/// (`auto`, the default) or name a binary to use.
fn xwayland_program() -> Option<PathBuf> {
    match env::var("MEOWLAND_XWAYLAND").ok().as_deref().map(str::trim) {
        Some("off" | "0" | "false" | "no") => None,
        Some(path) if !matches!(path, "" | "auto" | "on" | "1" | "true" | "yes") => {
            Some(PathBuf::from(path))
        }
        _ => env::var_os("PATH").and_then(|path| {
            env::split_paths(&path)
                .map(|directory| directory.join("xwayland-satellite"))
                .find(|candidate| candidate.is_file())
        }),
    }
}

fn start_xwayland(program: &Path, wayland: &str, log_path: &Path) -> Result<Xwayland> {
    let log = diag::open(log_path)?;
    // `xwayland-satellite` forwards only a subset of Xwayland's options and
    // does not accept `-displayfd`, so the display number is chosen here and
    // verified by waiting for the socket.  A number that loses a race makes
    // Xwayland exit at once; the next candidate is then tried.
    let mut last = String::new();
    for number in FIRST_X_DISPLAY..=LAST_X_DISPLAY {
        if x_display_in_use(number) {
            continue;
        }
        let mut child = Command::new(program)
            .arg(format!(":{number}"))
            .env("WAYLAND_DISPLAY", wayland)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        match wait_for_x_socket(number, &mut child, Duration::from_secs(5)) {
            Ok(true) => {
                return Ok(Xwayland {
                    child,
                    display: format!(":{number}"),
                });
            }
            Ok(false) => {
                last = format!(":{number}");
                let _ = child.wait();
            }
            Err(error) => {
                // Nothing is listening but the process is alive: this is a
                // broken satellite, not a lost race, so stop trying numbers
                // and take its tree down with it.
                terminate_tree(&HashSet::from([child.id()]));
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    bail!("no X display could be started (last tried {last})")
}

const FIRST_X_DISPLAY: u32 = 8;
const LAST_X_DISPLAY: u32 = 63;

fn x_display_socket(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{number}"))
}

/// Another X server owns the display if its lock is held by a live process or
/// its socket exists.
fn x_display_in_use(number: u32) -> bool {
    if x_display_socket(number).exists() {
        return true;
    }
    let Ok(lock) = fs::read_to_string(format!("/tmp/.X{number}-lock")) else {
        return false;
    };
    let pid = lock.trim().parse::<i32>().ok();
    let Some(pid) = pid.and_then(rustix::process::Pid::from_raw) else {
        return true;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

fn wait_for_x_socket(number: u32, child: &mut Child, timeout: Duration) -> Result<bool> {
    let socket = x_display_socket(number);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return Ok(false);
        }
        if socket.exists() && UnixStream::connect(&socket).is_ok() {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!(
        "X server did not listen on {} within {timeout:?}",
        socket.display()
    )
}
