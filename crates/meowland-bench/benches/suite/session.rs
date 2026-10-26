//! Local compositor harness, or full socket transport via
//! `MEOWLAND_BENCH_SERVER`.

use std::{
    collections::VecDeque,
    env, fs,
    hint::black_box,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{self, Child, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use meowland_compositor::{Command, Event, PaneId, Runtime};
use meowland_core::scale::UiScale;
use meowland_kitty::Presenter;
use meowland_protocol::{
    self as protocol, FrameAck, Hello, PaneAction, PaneToServer, Paths, ServerToPane,
};

pub const TIMEOUT: Duration = Duration::from_secs(10);
const CELL: (u16, u16) = (10, 20);

pub struct CommitLog {
    pub queue: VecDeque<(Instant, bool)>,
    pub latencies: Vec<Duration>,
    pub presented: usize,
    pub released: bool,
}

#[derive(Clone)]
pub enum Commands {
    Local(calloop::channel::Sender<Command>),
    Socket(Arc<Mutex<UnixStream>>),
}

impl Commands {
    pub fn send(&self, command: Command) -> Result<(), String> {
        match self {
            Self::Local(sender) => sender.send(command).map_err(|error| error.to_string()),
            Self::Socket(socket) => {
                let message = match command {
                    Command::Attach {
                        show,
                        width,
                        height,
                        ..
                    } => PaneToServer::Hello(Hello {
                        show,
                        width,
                        height,
                    }),
                    Command::Pane { action, .. } => PaneToServer::Action(action),
                    Command::Shutdown => return Ok(()),
                    _ => return Err("unsupported benchmark command".into()),
                };
                protocol::send(&mut *socket.lock().unwrap(), &message)
                    .map_err(|error| error.to_string())
            }
        }
    }
}

pub struct Session {
    pub commands: Commands,
    pub log: Arc<Mutex<CommitLog>>,
    display: String,
    runtime: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
    receiver: Option<thread::JoinHandle<()>>,
    server: Option<RemoteServer>,
}

impl Session {
    pub fn new() -> Result<Self, String> {
        if let Some(binary) = env::var_os("MEOWLAND_BENCH_SERVER") {
            return Self::remote(Path::new(&binary));
        }
        let display = format!("wayland-meowland-{}", process::id());
        if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
            let _ = fs::remove_file(Path::new(&runtime).join(&display));
        }
        let (commands, incoming) = calloop::channel::channel();
        let (events_tx, events) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let socket = display.clone();
        let thread = thread::spawn(move || {
            let mut runtime = match Runtime::new(&socket, UiScale::default()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if let Err(error) = runtime
                .handle()
                .insert_source(incoming, |event, (), compositor| match event {
                    calloop::channel::Event::Msg(command) => compositor.handle_command(command),
                    calloop::channel::Event::Closed => compositor.handle_command(Command::Shutdown),
                })
            {
                let _ = ready_tx.send(Err(std::io::Error::other(error.to_string())));
                return;
            }
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            while !runtime.compositor().stopped() {
                if runtime.dispatch(None).is_err() {
                    break;
                }
                for event in runtime.compositor().drain_events() {
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });
        ready
            .recv_timeout(TIMEOUT)
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        Ok(Self::receive(
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?),
            display,
            Commands::Local(commands),
            events.into_iter(),
            Some(thread),
            None,
        ))
    }

    fn remote(binary: &Path) -> Result<Self, String> {
        let runtime =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?);
        let directory = runtime.join(format!("meowland-bench-{}", process::id()));
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let paths = Paths::in_runtime_dir(&directory).map_err(|error| error.to_string())?;
        let child = match process::Command::new(binary)
            .env("XDG_RUNTIME_DIR", &directory)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir(&directory);
                return Err(format!("starting benchmark server: {error}"));
            }
        };
        let mut server = RemoteServer { child, directory };
        let display = format!("wayland-meowland-{}", server.child.id());
        let deadline = Instant::now() + TIMEOUT;
        while !paths.pane.exists() || !server.directory.join(&display).exists() {
            if server
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err("benchmark server exited during startup".into());
            }
            if Instant::now() >= deadline {
                return Err("benchmark server did not start".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        let mut reader = UnixStream::connect(paths.pane).map_err(|error| error.to_string())?;
        let writer = reader.try_clone().map_err(|error| error.to_string())?;
        writer
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|error| error.to_string())?;
        let events = std::iter::from_fn(move || {
            protocol::recv(&mut reader).ok().map(|message| Event {
                pane: PaneId(1),
                message,
            })
        });
        Ok(Self::receive(
            server.directory.clone(),
            display,
            Commands::Socket(Arc::new(Mutex::new(writer))),
            events,
            None,
            Some(server),
        ))
    }

    fn receive(
        runtime: PathBuf,
        display: String,
        commands: Commands,
        events: impl Iterator<Item = Event> + Send + 'static,
        thread: Option<thread::JoinHandle<()>>,
        server: Option<RemoteServer>,
    ) -> Self {
        let log = Arc::new(Mutex::new(CommitLog {
            queue: VecDeque::new(),
            latencies: Vec::new(),
            presented: 0,
            released: false,
        }));
        let receiver_log = log.clone();
        let receiver_commands = commands.clone();
        let receiver = thread::spawn(move || {
            let mut presenter = Presenter::new(Some(CELL), None);
            let mut terminal_output = Vec::new();
            for Event { pane, message } in events {
                match message {
                    ServerToPane::Frame(band) => {
                        presenter.present_into(band, &mut terminal_output);
                        black_box(terminal_output.len());
                        let now = Instant::now();
                        let mut log = receiver_log.lock().unwrap();
                        log.presented += 1;
                        let started = log
                            .queue
                            .drain(..)
                            .filter_map(|(started, record)| record.then_some(started))
                            .next_back();
                        if let Some(started) = started {
                            log.latencies.push(now.saturating_duration_since(started));
                        }
                        drop(log);
                        let _ = receiver_commands.send(Command::Pane {
                            pane,
                            action: PaneAction::Ack(FrameAck::Retained),
                        });
                    }
                    ServerToPane::Release(_) => {
                        receiver_log.lock().unwrap().released = true;
                        break;
                    }
                    _ => {}
                }
            }
        });
        Self {
            commands,
            display,
            runtime,
            thread,
            receiver: Some(receiver),
            log,
            server,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join(&self.display)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        self.server.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
    }
}

struct RemoteServer {
    child: Child,
    directory: PathBuf,
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().is_ok_and(|status| status.is_none()) {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
