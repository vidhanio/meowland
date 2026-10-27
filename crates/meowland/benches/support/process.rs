//! Isolated real server/pane processes, bounded waits, and deterministic cleanup.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::app::{App, Command as AppCommand};
use super::config::{Options, Size};
use super::endpoint::Terminal;
use super::fixture::{APP_ID, Pattern};
use super::record::{Event, Record};
use crate::{Result, fail};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

struct Process {
    child: Child,
    log: PathBuf,
}
impl Process {
    fn check(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            return Err(fail(format!(
                "child {} exited with {status}: {}",
                self.child.id(),
                self.diagnostics()
            )));
        }
        Ok(())
    }
    fn diagnostics(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) struct Session {
    app: Option<App>,
    pane: Option<Process>,
    terminal: Option<Terminal>,
    server: Process,
    _directory: tempfile::TempDir,
    events: mpsc::Receiver<Event>,
    pub record: Record,
}

fn command(binary: &Path, runtime: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("RUST_LOG", "warn")
        .env_remove("LISTEN_FDS")
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDNAMES")
        .env_remove("WAYLAND_SOCKET");
    command
}

impl Session {
    pub(crate) fn new(options: &Options, size: Size, pattern: Pattern) -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("meowland-e2e-").tempdir()?;
        let runtime = directory.path();
        let log = runtime.join("server.log");
        let child = command(&options.binary, runtime)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log)?)
            .spawn()?;
        let mut server = Process { child, log };
        let wayland = runtime.join(format!("wayland-meowland-{}", server.child.id()));
        let deadline = Instant::now() + TIMEOUT;
        while !wayland.exists() || !runtime.join("meowland-pane.sock").exists() {
            server.check()?;
            if Instant::now() >= deadline {
                return Err(fail(format!(
                    "server startup timed out: {}",
                    server.diagnostics()
                )));
            }
            thread::sleep(Duration::from_millis(1));
        }
        let (sender, events) = mpsc::channel();
        let (terminal, slave) = Terminal::start(
            size,
            options.transport,
            options.terminal_delay,
            sender.clone(),
        )?;
        let app = App::start(&wayland, size, pattern, sender)?;
        let log = runtime.join("pane.log");
        let mut pane = command(&options.binary, runtime);
        pane.arg("attach")
            .arg(APP_ID)
            .env("TERM", "xterm-kitty")
            .env("COLORTERM", "truecolor");
        let child = pty_process::blocking::Command::from(pane)
            .stderr(File::create(&log)?)
            .spawn(slave)?;
        let mut session = Self {
            app: Some(app),
            pane: Some(Process { child, log }),
            terminal: Some(terminal),
            server,
            _directory: directory,
            events,
            record: Record::default(),
        };
        session.wait("initial graphics and keyboard focus", |record| {
            record.ready && record.frames.contains_key(&0)
        })?;
        Ok(session)
    }

    pub(crate) fn command(&self, command: AppCommand) -> Result<()> {
        self.app
            .as_ref()
            .expect("live session owns app")
            .send(command)
    }

    pub(crate) fn inject(&self, bytes: &[u8]) -> Result<Instant> {
        self.terminal
            .as_ref()
            .expect("live session owns terminal")
            .inject(bytes)
    }

    pub(crate) fn wait(&mut self, label: &str, done: impl Fn(&Record) -> bool) -> Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.record) {
            self.pump(deadline, label)?;
        }
        Ok(())
    }

    pub(crate) fn pump(&mut self, deadline: Instant, label: &str) -> Result<()> {
        self.server.check()?;
        self.pane
            .as_mut()
            .expect("live session owns pane")
            .check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.diagnostic(&format!("timed out waiting for {label}")));
        }
        match self
            .events
            .recv_timeout(remaining.min(Duration::from_millis(100)))
        {
            Ok(event) => self
                .record
                .apply(event)
                .map_err(|error| self.diagnostic(&error.to_string())),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn drain(&mut self) -> Result<()> {
        self.server.check()?;
        self.pane
            .as_mut()
            .expect("live session owns pane")
            .check()?;
        for event in self.events.try_iter() {
            self.record.apply(event)?;
        }
        Ok(())
    }

    fn diagnostic(&self, error: &str) -> crate::Error {
        let pane = self
            .pane
            .as_ref()
            .map_or_else(String::new, Process::diagnostics);
        fail(format!(
            "{error}\nserver log: {}\npane log: {pane}",
            self.server.diagnostics()
        ))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.app.take();
        if let Some(pane) = &mut self.pane {
            let deadline = Instant::now() + Duration::from_millis(250);
            while matches!(pane.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
        }
        self.pane.take();
        self.terminal.take();
    }
}
