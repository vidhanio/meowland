use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
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
    writer: SyncSender<ServerToPane>,
    busy: bool,
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
                    if let Some(target) = panes.get_mut(&pane)
                        && !target.busy
                        && target
                            .writer
                            .try_send(ServerToPane::Frame { width, height, rgb })
                            .is_ok()
                    {
                        target.busy = true;
                    }
                }
                CompositorEvent::Release { pane, reason } => {
                    if let Some(target) = panes.remove(&pane) {
                        let _ = target.writer.try_send(ServerToPane::Release(reason));
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
                    ControlRequest::Run(args) => match launch_client(&args, &display, &paths.log) {
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
                let (send_tx, send_rx) = mpsc::sync_channel(1);
                let _ = protocol::send(&mut writer, &ServerToPane::HelloOk);
                thread::spawn(move || pane_writer(writer, send_rx));
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
                            let _ = target
                                .writer
                                .try_send(ServerToPane::Release("detached".into()));
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
    }

    let _ = commands.send(CompositorCommand::CloseAll);
    thread::sleep(Duration::from_millis(250));
    panes.clear();
    terminate_tree(&child_groups);
    let _ = commands.send(CompositorCommand::Shutdown);
    let _ = compositor_thread.join();
    let _ = fs::remove_file(&paths.control);
    let _ = fs::remove_file(&paths.pane);
    Ok(())
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

fn pane_writer(mut socket: UnixStream, rx: Receiver<ServerToPane>) {
    let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
    for message in rx {
        if protocol::send(&mut socket, &message).is_err() {
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

fn launch_client(args: &[String], display: &str, log_path: &PathBuf) -> Result<Child> {
    let Some(executable) = args.first() else {
        bail!("run requires a command");
    };
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let mut command = Command::new(executable);
    command
        .args(&args[1..])
        .env("WAYLAND_DISPLAY", display)
        .env_remove("DISPLAY")
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
