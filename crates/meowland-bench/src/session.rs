//! An isolated Meowland process tree whose pane writes to a pseudo-terminal
//! read by an [`Endpoint`].
//!
//! Each session owns private mode-0700 runtime, config, cache, and state
//! directories, strips activation and service variables from the
//! environment, and points D-Bus at an address nothing listens on, so the
//! user's own server, sockets, configuration, and desktop services are never
//! touched. Children run in their own sessions and receive `SIGTERM` if the
//! thread that spawned them dies. Dropping a [`Session`], including while
//! unwinding from a panic, stops the workload, sends `SIGTERM` to every owned
//! process, kills whatever is left after a grace period, reaps its children,
//! and unlinks shared-memory objects those processes still held.

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    fs::{self, File},
    io::{self, Read as _, Write as _},
    os::{
        fd::OwnedFd,
        unix::{
            net::{UnixListener, UnixStream},
            process::CommandExt as _,
        },
    },
    path::{Path, PathBuf},
    process::{self, Child, ExitStatus, Output, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{Mode, OFlags},
    io::Errno,
    process::{Pid, PidfdFlags, Signal},
    pty::OpenptFlags,
    termios::{OptionalActions, Winsize},
    time::Timespec,
};
use tempfile::TempDir;

use crate::{
    binaries::Binaries,
    control::{self, Command, Event, EventKind},
    endpoint::{Counters, Endpoint, Frame, Transport},
    pattern::{APP_ID, Pattern},
};

/// Deadline for one step: a socket appearing, a frame, an input response.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the pane to attach and show the workload's first frame.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Time between `SIGTERM` and `SIGKILL` during teardown.
const GRACE: Duration = Duration::from_secs(2);
/// Minimum time between terminal reads while throttled.
const THROTTLE_INTERVAL: Duration = Duration::from_millis(2);
/// Largest terminal read while throttled.
const THROTTLE_READ: usize = 16 * 1024;
/// Largest terminal read otherwise.
const READ_SIZE: usize = 256 * 1024;
/// Kitty keyboard protocol press and release of `a`.
const KEY_PRESS: &[u8] = b"\x1b[97;1:1u";
const KEY_RELEASE: &[u8] = b"\x1b[97;1:3u";
/// Terminal cell size the endpoint reports, in pixels.
const CELL: (usize, usize) = (10, 20);

/// How the pane comes to show the workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    /// `meowland launch` runs the workload from inside the pane.
    Launch,
    /// `meowland launch` runs detached from any terminal, then
    /// `meowland focus --follow` shows the window in the pane.
    Focus,
}

/// What a session runs.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Workload and pane width in pixels.
    pub width: usize,
    /// Workload and pane height in pixels.
    pub height: usize,
    /// Workload content.
    pub pattern: Pattern,
    /// Image transports the terminal accepts.
    pub transport: Transport,
    /// How the pane attaches.
    pub attach: Attach,
}

/// A window as printed by `meowland list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Unique window ID.
    pub id: String,
    /// The toplevel's app ID.
    pub app_id: String,
    /// The toplevel's title.
    pub title: String,
    /// Whether the window is the active one.
    pub active: bool,
}

#[derive(Debug)]
struct Process {
    name: &'static str,
    child: Child,
    /// Whether exiting is a failure rather than expected.
    required: bool,
}

/// A running server, pane, and workload behind a headless terminal.
#[derive(Debug)]
pub struct Session {
    binaries: Binaries,
    root: TempDir,
    environment: Vec<(OsString, OsString)>,
    processes: Vec<Process>,
    master: OwnedFd,
    terminal: Option<OwnedFd>,
    listener: UnixListener,
    control: Option<UnixStream>,
    control_pending: Vec<u8>,
    closing: bool,
    endpoint: Endpoint,
    buffer: Vec<u8>,
    replies: Vec<u8>,
    fresh: Vec<Frame>,
    events: HashMap<(EventKind, u64), Duration>,
    frames: HashMap<u64, Duration>,
    deliveries: Vec<Frame>,
    app: Option<u32>,
    ready: bool,
    throttled: bool,
    next_read: Duration,
}

impl Session {
    /// Start a server, attach a pane to the workload, and wait until the
    /// pane shows its verified first frame with keyboard focus.
    ///
    /// # Errors
    /// Returns an error if any process fails to start or a step times out;
    /// whatever was started is torn down.
    pub fn start(binaries: &Binaries, config: Config) -> anyhow::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("meowland-bench-")
            .tempdir()?;
        for directory in ["config", "cache", "state"] {
            fs::create_dir(root.path().join(directory))?;
        }
        let environment = isolated_environment(root.path());
        let (master, terminal) = open_terminal(config.width, config.height)?;
        let socket = root.path().join("workload.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let server = spawn(
            &binaries.meowland,
            ["server"],
            &environment,
            root.path(),
            "server",
            (Stdio::null(), Stdio::null()),
        )?;
        let mut session = Self {
            binaries: binaries.clone(),
            root,
            environment,
            processes: vec![server],
            master,
            terminal: Some(terminal),
            listener,
            control: None,
            control_pending: Vec::new(),
            closing: false,
            endpoint: Endpoint::new(
                config.width,
                config.height,
                config.pattern,
                config.transport,
            ),
            buffer: vec![0; READ_SIZE],
            replies: Vec::new(),
            fresh: Vec::new(),
            events: HashMap::new(),
            frames: HashMap::new(),
            deliveries: Vec::new(),
            app: None,
            ready: false,
            throttled: false,
            next_read: Duration::ZERO,
        };
        session.until("server sockets", STEP_TIMEOUT, |session| {
            ["meowland-control.sock", "meowland-pane.sock"]
                .iter()
                .all(|name| session.root.path().join(name).exists())
        })?;
        let launch = [
            OsStr::new("launch"),
            "--app-id".as_ref(),
            APP_ID.as_ref(),
            "--".as_ref(),
            binaries.workload.as_os_str(),
            socket.as_os_str(),
            config.width.to_string().as_ref(),
            config.height.to_string().as_ref(),
            config.pattern.name().as_ref(),
        ]
        .map(OsStr::to_owned);
        let pane = match config.attach {
            Attach::Launch => launch.to_vec(),
            Attach::Focus => {
                session.launch_detached(&launch)?;
                ["focus", "--follow", "--app-id", APP_ID]
                    .map(OsString::from)
                    .to_vec()
            }
        };
        let terminal = session
            .terminal
            .take()
            .context("terminal already attached")?;
        let pane = spawn(
            &session.binaries.meowland,
            pane,
            &session.environment,
            session.root.path(),
            "pane",
            (Stdio::from(terminal.try_clone()?), Stdio::from(terminal)),
        )?;
        session.processes.push(pane);
        session.until(
            "initial frame and keyboard focus",
            START_TIMEOUT,
            |session| session.ready && session.frames.contains_key(&0),
        )?;
        Ok(session)
    }

    /// Run `meowland launch` without a terminal, which only asks the server
    /// to start the workload.
    fn launch_detached(&mut self, launch: &[OsString]) -> anyhow::Result<()> {
        let mut launcher = spawn(
            &self.binaries.meowland,
            launch,
            &self.environment,
            self.root.path(),
            "launch",
            (Stdio::null(), Stdio::null()),
        )?;
        launcher.required = false;
        self.processes.push(launcher);
        let index = self.processes.len() - 1;
        let deadline = control::now() + STEP_TIMEOUT;
        loop {
            if let Some(status) = self.processes[index].child.try_wait()? {
                ensure!(
                    status.success(),
                    "detached meowland launch failed with {status}\n{}",
                    self.diagnostics()
                );
                return Ok(());
            }
            ensure!(
                control::now() < deadline,
                "detached meowland launch did not return\n{}",
                self.diagnostics()
            );
            self.pump(Duration::from_millis(10))?;
        }
    }

    /// Endpoint totals so far.
    pub const fn counters(&self) -> Counters {
        self.endpoint.counters()
    }

    /// When the workload committed `frame`, once it has reported it.
    pub fn committed(&self, frame: u64) -> Option<Duration> {
        self.event(EventKind::Commit, frame)
    }

    /// When the endpoint consumed `frame`, if it has.
    pub fn consumed(&self, frame: u64) -> Option<Duration> {
        self.frames.get(&frame).copied()
    }

    /// Every frame consumed so far, in order.
    pub fn deliveries(&self) -> &[Frame] {
        &self.deliveries
    }

    fn event(&self, kind: EventKind, id: u64) -> Option<Duration> {
        self.events.get(&(kind, id)).copied()
    }

    /// Limit terminal reads to 16 KiB every 2 ms from now on, emulating a
    /// terminal that cannot keep up.
    pub const fn throttle(&mut self) {
        self.throttled = true;
    }

    /// Send `command` to the workload.
    ///
    /// # Errors
    /// Returns an error if the workload has not connected or the write
    /// fails.
    pub fn command(&mut self, command: Command) -> anyhow::Result<()> {
        let control = self
            .control
            .as_mut()
            .context("workload has not connected")?;
        writeln!(control, "{command}").context("writing a workload command")
    }

    /// Commit `frame` and wait for the endpoint to consume it, returning the
    /// time from commit to consumption.
    ///
    /// # Errors
    /// Returns an error if the frame does not arrive in time or the session
    /// fails.
    pub fn frame(&mut self, frame: u64) -> anyhow::Result<Duration> {
        self.command(Command::Commit(frame))?;
        self.until(format_args!("frame {frame}"), STEP_TIMEOUT, |session| {
            session.committed(frame).is_some() && session.consumed(frame).is_some()
        })?;
        elapsed(self.committed(frame), self.consumed(frame))
    }

    /// Press `a` in the terminal and wait for the workload's response,
    /// `frame`, to be consumed, returning the time from writing the key to
    /// consumption.
    ///
    /// # Errors
    /// Returns an error if the input or frame does not arrive in time, the
    /// timestamps are inconsistent, or the session fails.
    pub fn input(&mut self, frame: u64) -> anyhow::Result<Duration> {
        self.command(Command::Arm(frame))?;
        self.until(
            format_args!("arming input {frame}"),
            STEP_TIMEOUT,
            |session| session.event(EventKind::Armed, frame).is_some(),
        )?;
        let start = control::now();
        self.write_terminal(KEY_PRESS)?;
        self.until(
            format_args!("input response {frame}"),
            STEP_TIMEOUT,
            |session| {
                session.event(EventKind::Input, frame).is_some()
                    && session.committed(frame).is_some()
                    && session.consumed(frame).is_some()
            },
        )?;
        self.write_terminal(KEY_RELEASE)?;
        let delivery = elapsed(Some(start), self.event(EventKind::Input, frame))?;
        let response = elapsed(Some(start), self.consumed(frame))?;
        ensure!(
            delivery <= response,
            "frame {frame} consumed before its input arrived"
        );
        Ok(response)
    }

    /// Commit `content`'s pixels again under ID `frame` and wait for the
    /// compositor to release the buffer, returning the time from commit to
    /// release as the workload observed it.
    ///
    /// # Errors
    /// Returns an error if the release does not arrive in time or the
    /// session fails.
    pub fn recommit(&mut self, frame: u64, content: u64) -> anyhow::Result<Duration> {
        self.command(Command::Repeat { frame, content })?;
        self.until(
            format_args!("release of {frame}"),
            STEP_TIMEOUT,
            |session| {
                session.committed(frame).is_some()
                    && session.event(EventKind::Release, frame).is_some()
            },
        )?;
        elapsed(self.committed(frame), self.event(EventKind::Release, frame))
    }

    /// Process output and events until `done` holds.
    ///
    /// # Errors
    /// Returns an error on timeout or session failure.
    pub fn until(
        &mut self,
        label: impl fmt::Display,
        timeout: Duration,
        mut done: impl FnMut(&Self) -> bool,
    ) -> anyhow::Result<()> {
        let deadline = control::now() + timeout;
        while !done(self) {
            let now = control::now();
            ensure!(
                now < deadline,
                "timed out after {timeout:?} waiting for {label}\n{}",
                self.diagnostics()
            );
            self.pump(deadline.saturating_sub(now).min(Duration::from_millis(20)))?;
        }
        Ok(())
    }

    /// Process output and events for `duration`.
    ///
    /// # Errors
    /// Returns an error on session failure.
    pub fn settle(&mut self, duration: Duration) -> anyhow::Result<()> {
        let deadline = control::now() + duration;
        loop {
            let now = control::now();
            if now >= deadline {
                return Ok(());
            }
            self.pump(deadline.saturating_sub(now).min(Duration::from_millis(10)))?;
        }
    }

    /// Wait up to `timeout` for terminal output or workload events and
    /// process them.
    ///
    /// # Errors
    /// Returns an error if a required process exited, the workload
    /// disconnected unexpectedly, or the terminal output is invalid.
    pub fn pump(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.check()?;
        let now = control::now();
        let reading = !self.throttled || now >= self.next_read;
        let timeout = if reading {
            timeout
        } else {
            timeout.min(self.next_read.saturating_sub(now))
        };
        let timeout = Timespec::try_from(timeout)?;
        let accepting = if self.control.is_none() && !self.closing {
            PollFlags::IN
        } else {
            PollFlags::empty()
        };
        let output = if reading {
            PollFlags::IN
        } else {
            PollFlags::empty()
        };
        let listener = PollFd::new(&self.listener, accepting);
        let terminal = PollFd::new(&self.master, output);
        let result = if let Some(control) = &self.control {
            let mut descriptors = [listener, terminal, PollFd::new(control, PollFlags::IN)];
            poll(&mut descriptors, Some(&timeout))
                .map(|_| descriptors.map(|descriptor| descriptor.revents()))
        } else {
            let mut descriptors = [listener, terminal];
            poll(&mut descriptors, Some(&timeout)).map(|_| {
                let [listener, terminal] = descriptors.map(|descriptor| descriptor.revents());
                [listener, terminal, PollFlags::empty()]
            })
        };
        let [listener, terminal, control] = match result {
            Ok(events) => events,
            Err(Errno::INTR) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if listener.contains(PollFlags::IN) {
            self.accept()?;
        }
        if !control.is_empty() {
            self.read_control()?;
        }
        if reading && !terminal.is_empty() {
            self.read_terminal()?;
        }
        Ok(())
    }

    fn accept(&mut self) -> anyhow::Result<()> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                self.control = Some(stream);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn read_control(&mut self) -> anyhow::Result<()> {
        let Some(control) = &mut self.control else {
            return Ok(());
        };
        let mut bytes = [0; 4096];
        let count = control.read(&mut bytes)?;
        if count == 0 {
            self.control = None;
            ensure!(
                self.closing,
                "workload control socket closed\n{}",
                self.diagnostics()
            );
            return Ok(());
        }
        self.control_pending.extend_from_slice(&bytes[..count]);
        while let Some(end) = self.control_pending.iter().position(|&byte| byte == b'\n') {
            let event: Event = std::str::from_utf8(&self.control_pending[..end])?.parse()?;
            self.control_pending.drain(..=end);
            match event.kind {
                EventKind::Pid => self.app = Some(u32::try_from(event.id)?),
                EventKind::Ready => self.ready = true,
                _ => {}
            }
            self.events
                .entry((event.kind, event.id))
                .or_insert(event.at);
        }
        ensure!(
            self.control_pending.len() <= 4096,
            "oversized workload event"
        );
        Ok(())
    }

    fn read_terminal(&mut self) -> anyhow::Result<()> {
        let limit = if self.throttled {
            THROTTLE_READ
        } else {
            READ_SIZE
        };
        loop {
            let count = match rustix::io::read(&self.master, &mut self.buffer[..limit]) {
                Ok(0) | Err(Errno::IO) => {
                    bail!("pane terminal closed\n{}", self.diagnostics())
                }
                Ok(count) => count,
                Err(Errno::AGAIN) => break,
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            };
            let fed = self
                .endpoint
                .feed(&self.buffer[..count], &mut self.replies, &mut self.fresh);
            if let Err(error) = fed {
                bail!("{error:#}\n{}", self.diagnostics());
            }
            for frame in self.fresh.drain(..) {
                self.frames.entry(frame.id).or_insert(frame.at);
                self.deliveries.push(frame);
            }
            let replies = std::mem::take(&mut self.replies);
            self.write_terminal(&replies)?;
            self.replies = replies;
            self.replies.clear();
            if self.throttled {
                self.next_read = control::now() + THROTTLE_INTERVAL;
                break;
            }
        }
        Ok(())
    }

    fn write_terminal(&self, mut bytes: &[u8]) -> anyhow::Result<()> {
        let deadline = control::now() + Duration::from_secs(1);
        while !bytes.is_empty() {
            match rustix::io::write(&self.master, bytes) {
                Ok(count) => bytes = &bytes[count..],
                Err(Errno::AGAIN) => {
                    ensure!(
                        control::now() < deadline,
                        "pane stopped reading its terminal"
                    );
                    let mut descriptors = [PollFd::new(&self.master, PollFlags::OUT)];
                    match poll(
                        &mut descriptors,
                        Some(&Timespec::try_from(Duration::from_millis(10))?),
                    ) {
                        Ok(_) | Err(Errno::INTR) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn check(&mut self) -> anyhow::Result<()> {
        for index in 0..self.processes.len() {
            let process = &mut self.processes[index];
            if process.required
                && let Some(status) = process.child.try_wait()?
            {
                let name = process.name;
                bail!("{name} exited with {status}\n{}", self.diagnostics());
            }
        }
        Ok(())
    }

    /// State and log tails for failure messages.
    fn diagnostics(&self) -> String {
        let mut text = format!(
            "ready={} app={:?} commits={} frames={} counters={:?}",
            self.ready,
            self.app,
            self.events
                .keys()
                .filter(|(kind, _)| *kind == EventKind::Commit)
                .count(),
            self.frames.len(),
            self.endpoint.counters()
        );
        for process in &self.processes {
            let log = self.root.path().join(format!("{}.log", process.name));
            let content = fs::read(&log).unwrap_or_default();
            let tail = &content[content.len().saturating_sub(4096)..];
            if !tail.is_empty() {
                let _ = write!(
                    text,
                    "\n--- {} stderr ---\n{}",
                    process.name,
                    String::from_utf8_lossy(tail).trim_end()
                );
            }
        }
        text
    }

    /// Run the `meowland` client with the session's environment.
    ///
    /// # Errors
    /// Returns an error if it cannot run or exits unsuccessfully.
    fn meowland<I: AsRef<OsStr>>(
        &self,
        args: impl IntoIterator<Item = I>,
    ) -> anyhow::Result<Output> {
        let output = process::Command::new(&self.binaries.meowland)
            .args(args)
            .env_clear()
            .envs(self.environment.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            output.status.success(),
            "meowland client failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
        Ok(output)
    }

    /// The server's windows, from `meowland list`.
    ///
    /// # Errors
    /// Returns an error if listing fails.
    pub fn windows(&self) -> anyhow::Result<Vec<Window>> {
        let output = self.meowland(["list"])?;
        String::from_utf8(output.stdout)?
            .lines()
            .map(|line| {
                let mut fields = line.split('\t');
                let mut field = || fields.next().map(str::to_owned);
                Ok(Window {
                    id: field().context("missing window ID")?,
                    app_id: field().unwrap_or_default(),
                    title: field().unwrap_or_default(),
                    active: field().as_deref() == Some("active"),
                })
            })
            .collect()
    }

    /// Ask the window `id` to close with `meowland quit`; the workload
    /// exiting afterwards is no longer a failure.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub fn quit(&mut self, id: &str) -> anyhow::Result<()> {
        self.closing = true;
        self.meowland(["quit", id])?;
        Ok(())
    }

    /// Send the server `SIGTERM` and wait up to `timeout` for it to exit.
    ///
    /// # Errors
    /// Returns an error if signalling fails or the server outlives the
    /// timeout.
    pub fn stop_server(&mut self, timeout: Duration) -> anyhow::Result<ExitStatus> {
        let server = &mut self.processes[0];
        server.required = false;
        rustix::process::kill_process(Pid::from_child(&server.child), Signal::TERM)?;
        let deadline = control::now() + timeout;
        loop {
            if let Some(status) = server.child.try_wait()? {
                return Ok(status);
            }
            ensure!(
                control::now() < deadline,
                "server ignored SIGTERM for {timeout:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn teardown(&mut self) {
        let mut owned = HashSet::new();
        for process in &mut self.processes {
            if matches!(process.child.try_wait(), Ok(None)) {
                owned.insert(process.child.id());
                descendants(process.child.id(), &mut owned);
            }
        }
        if let Some(app) = self.app
            && fs::read_link(format!("/proc/{app}/exe"))
                .is_ok_and(|exe| exe == self.binaries.workload)
        {
            owned.insert(app);
        }
        let handles: Vec<OwnedFd> = owned
            .iter()
            .filter_map(|&pid| {
                let pid = Pid::from_raw(i32::try_from(pid).ok()?)?;
                rustix::process::pidfd_open(pid, PidfdFlags::empty()).ok()
            })
            .collect();
        let shared: HashSet<PathBuf> = owned.iter().flat_map(|&pid| shared_objects(pid)).collect();
        if let Some(control) = &mut self.control {
            drop(writeln!(control, "{}", Command::Stop));
        }
        signal(&handles, Signal::TERM);
        let deadline = control::now() + GRACE;
        while control::now() < deadline
            && self
                .processes
                .iter_mut()
                .any(|process| matches!(process.child.try_wait(), Ok(None)))
        {
            thread::sleep(Duration::from_millis(10));
        }
        signal(&handles, Signal::KILL);
        for process in &mut self.processes {
            drop(process.child.wait());
        }
        for path in shared {
            drop(fs::remove_file(path));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn elapsed(start: Option<Duration>, end: Option<Duration>) -> anyhow::Result<Duration> {
    let (start, end) = start.zip(end).context("missing timestamp")?;
    end.checked_sub(start)
        .with_context(|| format!("event at {end:?} precedes its cause at {start:?}"))
}

/// The harness's environment without anything that could reach the user's
/// session, plus private XDG directories under `root`.
fn isolated_environment(root: &Path) -> Vec<(OsString, OsString)> {
    const REMOVED: [&str; 6] = [
        "NOTIFY_SOCKET",
        "WAYLAND_SOCKET",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_ACTIVATION_TOKEN",
        "DESKTOP_STARTUP_ID",
    ];
    let mut bus = OsString::from("unix:path=");
    bus.push(root.join("no-bus"));
    let set: [(&str, OsString); 9] = [
        ("XDG_RUNTIME_DIR", root.into()),
        ("XDG_CONFIG_HOME", root.join("config").into()),
        ("XDG_CACHE_HOME", root.join("cache").into()),
        ("XDG_STATE_HOME", root.join("state").into()),
        ("DBUS_SESSION_BUS_ADDRESS", bus),
        ("NO_AT_BRIDGE", "1".into()),
        ("RUST_LOG", "warn".into()),
        ("TERM", "xterm-kitty".into()),
        ("COLORTERM", "truecolor".into()),
    ];
    let mut environment: Vec<_> = env::vars_os()
        .filter(|(key, _)| {
            key.to_str().is_none_or(|key| {
                !["LISTEN_", "WATCHDOG_", "MEOWLAND_"]
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
                    && !REMOVED.contains(&key)
                    && !set.iter().any(|(name, _)| *name == key)
            })
        })
        .collect();
    environment.extend(set.into_iter().map(|(key, value)| (key.into(), value)));
    environment
}

/// A raw pseudo-terminal sized `width`×`height` pixels, returning its
/// non-blocking controller and the terminal for the pane.
fn open_terminal(width: usize, height: usize) -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let master =
        rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let terminal = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut termios = rustix::termios::tcgetattr(&terminal)?;
    termios.make_raw();
    rustix::termios::tcsetattr(&terminal, OptionalActions::Now, &termios)?;
    rustix::termios::tcsetwinsize(
        &terminal,
        Winsize {
            ws_row: u16::try_from(height / CELL.1)?,
            ws_col: u16::try_from(width / CELL.0)?,
            ws_xpixel: u16::try_from(width)?,
            ws_ypixel: u16::try_from(height)?,
        },
    )?;
    rustix::io::ioctl_fionbio(&master, true)?;
    Ok((master, terminal))
}

/// Start `program` in its own session with `environment`, logging stderr to
/// `name.log` under `logs`.
fn spawn<I: AsRef<OsStr>>(
    program: &Path,
    args: impl IntoIterator<Item = I>,
    environment: &[(OsString, OsString)],
    logs: &Path,
    name: &'static str,
    (stdin, stdout): (Stdio, Stdio),
) -> anyhow::Result<Process> {
    let log = File::create(logs.join(format!("{name}.log")))?;
    let parent = rustix::process::getpid();
    let mut command = process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(log);
    // SAFETY: the hook runs between fork and exec and only makes the
    // async-signal-safe setsid, prctl, and getppid system calls, building
    // its error without allocating.
    unsafe {
        command.pre_exec(move || {
            rustix::process::setsid()?;
            rustix::process::set_parent_process_death_signal(Some(Signal::TERM))?;
            if rustix::process::getppid() == Some(parent) {
                Ok(())
            } else {
                Err(Errno::SRCH.into())
            }
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    Ok(Process {
        name,
        child,
        required: true,
    })
}

/// Add every descendant of `pid`, across all of its threads, to `found`.
fn descendants(pid: u32, found: &mut HashSet<u32>) {
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        let Ok(tasks) = fs::read_dir(format!("/proc/{parent}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children
                .split_whitespace()
                .filter_map(|child| child.parse().ok())
            {
                if found.insert(child) {
                    pending.push(child);
                }
            }
        }
    }
}

/// Meowland shared-memory objects `pid` holds open.
fn shared_objects(pid: u32) -> Vec<PathBuf> {
    let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    descriptors
        .flatten()
        .filter_map(|descriptor| {
            let target = fs::read_link(descriptor.path()).ok()?;
            let target = target.to_str()?;
            let target = target.strip_suffix(" (deleted)").unwrap_or(target);
            target
                .starts_with("/dev/shm/meowland-")
                .then(|| PathBuf::from(target))
        })
        .collect()
}

fn signal(handles: &[OwnedFd], signal: Signal) {
    for handle in handles {
        let _ = rustix::process::pidfd_send_signal(handle, signal);
    }
}
