use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write as _},
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

use crate::{
    compositor::{self, Command as CompositorCommand, Event as CompositorEvent},
    protocol::{
        self, ControlRequest, ControlResponse, Hello, PaneToServer, ServerToPane, WindowInfo,
    },
};

#[derive(Clone, Debug)]
pub struct Paths {
    pub control: PathBuf,
    pub pane: PathBuf,
    pub log: PathBuf,
}

impl Paths {
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
            log: env::var_os("MEOWLAND_LOG")
                .map_or_else(|| runtime.join("meowland.log"), PathBuf::from),
        })
    }
}

enum Incoming {
    Control(ControlRequest, Sender<ControlResponse>),
    Pane(Hello, UnixStream),
    PaneMessage(u64, PaneToServer),
    PaneGone(u64),
}

struct Pane {
    writer: Sender<ServerToPane>,
    busy: bool,
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

#[allow(clippy::too_many_lines)]
pub fn serve(paths: &Paths) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))?;
    }
    let control = bind(&paths.control)?;
    let pane = bind(&paths.pane)?;
    let (commands, events, display, compositor_thread) = compositor::spawn()?;
    let mut xwayland = xwayland_program().and_then(|program| {
        match start_xwayland(&program, &display, &paths.log) {
            Ok(xwayland) => {
                log_line(
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
                log_line(&paths.log, &format!("xwayland: not started: {error:#}"));
                None
            }
        }
    });
    let (incoming_tx, incoming_rx) = mpsc::channel();
    accept_control(control, incoming_tx.clone());
    accept_panes(pane, incoming_tx.clone());
    let mut next_pane = 1u64;
    let mut panes: HashMap<u64, Pane> = HashMap::new();
    let mut windows: HashMap<u64, WindowInfo> = HashMap::new();
    let mut children: Vec<Child> = Vec::new();
    let mut child_groups = HashSet::new();
    let mut stopping = false;

    while !stopping && !interrupted.load(Ordering::Relaxed) {
        while let Ok(event) = events.try_recv() {
            match event {
                CompositorEvent::WindowUp(info) => {
                    windows.insert(info.id, info);
                }
                CompositorEvent::WindowDown(id) => {
                    windows.remove(&id);
                }
                CompositorEvent::Title { pane, title } => {
                    if let Some(target) = panes.get(&pane) {
                        let _ = target.writer.send(ServerToPane::Title(title));
                    }
                }
                CompositorEvent::Cursor { pane, shape } => {
                    if let Some(target) = panes.get(&pane) {
                        let _ = target.writer.send(ServerToPane::Cursor(shape));
                    }
                }
                CompositorEvent::Focus(id) => {
                    for window in windows.values_mut() {
                        window.active = window.id == id;
                    }
                }
                CompositorEvent::Frame {
                    pane,
                    width,
                    height,
                    rgb,
                } => {
                    let frame = ServerToPane::Frame { width, height, rgb };
                    match panes.get_mut(&pane).filter(|target| !target.busy) {
                        Some(target) => match target.writer.send(frame) {
                            Ok(()) => target.busy = true,
                            Err(error) => recycle(&commands, error.0),
                        },
                        None => recycle(&commands, frame),
                    }
                }
                CompositorEvent::Release { pane, reason } => {
                    if let Some(target) = panes.remove(&pane) {
                        release(&target.writer, reason);
                    }
                }
            }
        }
        match incoming_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Incoming::Control(request, reply)) => {
                let response = match request {
                    ControlRequest::Ping => ControlResponse::Ok,
                    ControlRequest::List => {
                        let mut list: Vec<_> = windows.values().cloned().collect();
                        list.sort_by_key(|window| window.id);
                        ControlResponse::Windows(list)
                    }
                    ControlRequest::Run(args) => match launch_client(
                        &args,
                        &display,
                        xwayland.as_ref().map(|xwayland| xwayland.display.as_str()),
                        &paths.log,
                    ) {
                        Ok(child) => {
                            child_groups.insert(child.id());
                            children.push(child);
                            ControlResponse::Ok
                        }
                        Err(error) => ControlResponse::Error(format!("{error:#}")),
                    },
                    ControlRequest::Stop => {
                        stopping = true;
                        ControlResponse::Ok
                    }
                };
                let _ = reply.send(response);
            }
            Ok(Incoming::Pane(hello, socket)) => {
                let mut writer = socket.try_clone()?;
                if hello.version != protocol::VERSION {
                    let _ = protocol::send(
                        &mut writer,
                        &ServerToPane::Reject(format!(
                            "pane protocol version {} is unsupported; server uses {}",
                            hello.version,
                            protocol::VERSION
                        )),
                    );
                    continue;
                }
                if !valid_size(hello.width, hello.height) {
                    let _ = protocol::send(
                        &mut writer,
                        &ServerToPane::Reject("invalid pane dimensions".into()),
                    );
                    continue;
                }
                let id = next_pane;
                next_pane += 1;
                let (send_tx, send_rx) = mpsc::channel();
                let _ = protocol::send(&mut writer, &ServerToPane::HelloOk);
                let recycler = commands.clone();
                thread::spawn(move || pane_writer(writer, send_rx, recycler));
                read_pane(id, socket, incoming_tx.clone());
                panes.insert(
                    id,
                    Pane {
                        writer: send_tx,
                        busy: false,
                    },
                );
                let _ = commands.send(CompositorCommand::Attach {
                    pane: id,
                    show: hello.show,
                    width: hello.width,
                    height: hello.height,
                });
            }
            Ok(Incoming::PaneMessage(id, message)) => match message {
                PaneToServer::Hello(_) => {}
                PaneToServer::Input(event) => match &event {
                    protocol::Input::Key {
                        code: 17,
                        pressed: true,
                        modifiers,
                    } if modifiers & 0b1110 == 0b0100 => {
                        if let Some(target) = panes.remove(&id) {
                            release(&target.writer, "detached".into());
                        }
                        let _ = commands.send(CompositorCommand::Detach { pane: id });
                    }
                    protocol::Input::Key {
                        code: 16,
                        pressed: true,
                        modifiers,
                    } if modifiers & 0b1110 == 0b0100 => {
                        let _ = commands.send(CompositorCommand::CloseShown { pane: id });
                    }
                    protocol::Input::Key {
                        code: 16 | 17,
                        pressed: false,
                        modifiers,
                    } if modifiers & 0b1110 == 0b0100 => {}
                    _ => {
                        let _ = commands.send(CompositorCommand::Input { pane: id, event });
                    }
                },
                PaneToServer::Resize { width, height, .. } => {
                    if valid_size(width, height) {
                        let _ = commands.send(CompositorCommand::Resize {
                            pane: id,
                            width,
                            height,
                        });
                    }
                }
                PaneToServer::Ack => {
                    if let Some(pane) = panes.get_mut(&id) {
                        pane.busy = false;
                        let _ = commands.send(CompositorCommand::Ack { pane: id });
                    }
                }
            },
            Ok(Incoming::PaneGone(id)) => {
                panes.remove(&id);
                let _ = commands.send(CompositorCommand::Detach { pane: id });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        children.retain_mut(|child| child.try_wait().ok().flatten().is_none());
        if xwayland
            .as_mut()
            .is_some_and(|running| matches!(running.child.try_wait(), Ok(Some(_))))
        {
            log_line(&paths.log, "xwayland: server exited");
            xwayland = None;
        }
    }

    let _ = commands.send(CompositorCommand::CloseAll);
    thread::sleep(Duration::from_millis(250));
    panes.clear();
    if let Some(mut xwayland) = xwayland {
        terminate_tree(&HashSet::from([xwayland.child.id()]));
        let _ = xwayland.child.wait();
    }
    terminate_tree(&child_groups);
    let _ = commands.send(CompositorCommand::Shutdown);
    let _ = compositor_thread.join();
    let _ = fs::remove_file(&paths.control);
    let _ = fs::remove_file(&paths.pane);
    Ok(())
}

/// Tell a pane why it is being let go.  The pane may still have a frame in
/// flight, so this is queued behind it rather than dropped when the channel
/// happens to be full.  Frames are paced by the pane's acks, so the queue
/// holds at most one frame plus a few short messages.
fn release(writer: &Sender<ServerToPane>, reason: String) {
    let _ = writer.send(ServerToPane::Release(reason));
}

fn valid_size(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= 8192
        && height <= 8192
        && u64::from(width) * u64::from(height) <= 16_000_000
}

fn bind(path: &PathBuf) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok(listener)
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                bail!("server already listening at {}", path.display());
            }
            fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            let listener = UnixListener::bind(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok(listener)
        }
        Err(error) => Err(error.into()),
    }
}

fn accept_control(listener: UnixListener, incoming: Sender<Incoming>) {
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
                            if incoming.send(Incoming::Control(request, reply_tx)).is_ok()
                                && let Ok(reply) = reply_rx.recv_timeout(Duration::from_secs(2))
                            {
                                let _ = protocol::send(&mut socket, &reply);
                            }
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
}

fn accept_panes(listener: UnixListener, incoming: Sender<Incoming>) {
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    let incoming = incoming.clone();
                    thread::spawn(move || {
                        let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
                        if let Ok(PaneToServer::Hello(hello)) = protocol::recv(&mut socket) {
                            let _ = socket.set_read_timeout(None);
                            let _ = incoming.send(Incoming::Pane(hello, socket));
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
}

/// Owns `recycle` for the thread's whole life, so it is taken by value.
#[allow(clippy::needless_pass_by_value)]
fn pane_writer(
    mut socket: UnixStream,
    rx: Receiver<ServerToPane>,
    recycle: Sender<CompositorCommand>,
) {
    let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
    for message in rx {
        if protocol::send(&mut socket, &message).is_err() {
            break;
        }
        if let ServerToPane::Frame { rgb, .. } = message {
            let _ = recycle.send(CompositorCommand::Recycle(rgb));
        }
    }
}

/// Give a frame buffer back to the compositor for its next frame.
fn recycle(commands: &Sender<CompositorCommand>, message: ServerToPane) {
    if let ServerToPane::Frame { rgb, .. } = message {
        let _ = commands.send(CompositorCommand::Recycle(rgb));
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
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
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

fn terminate_tree(roots: &HashSet<u32>) {
    for signal in ["HUP", "TERM", "KILL"] {
        let descendants = descendants(roots);
        for pid in descendants.into_iter().chain(roots.iter().copied()) {
            let _ = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(pid.to_string())
                .status();
        }
        for group in roots {
            let _ = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg("--")
                .arg(format!("-{group}"))
                .status();
        }
        thread::sleep(Duration::from_millis(250));
    }
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
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
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

fn log_line(path: &Path, message: &str) {
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let _ = writeln!(file, "{seconds} {message}");
}
