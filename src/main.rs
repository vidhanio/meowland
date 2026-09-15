//! meowland: a Wayland compositor that runs inside your terminal.
//!
//! Clients connect to a Wayland socket like they always do. Their windows are
//! rendered into a frame buffer the compositor owns, that frame buffer is
//! diffed against the previous frame, and the differences are sent to the
//! terminal as [kitty graphics protocol][spec] images, tiled into
//! the cell grid. Keyboard and mouse input goes the other way.
//!
//! [spec]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

mod compositor;
mod keys;
mod kitty;
mod render;
mod shm;
mod tty;

use std::{
    fs::File,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use smithay::{
    reexports::{
        calloop::{
            EventLoop as Calloop, Interest, LoopSignal, Mode, PostAction,
            channel::{Event as ChannelEvent, Sender, channel},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::Display,
    },
    utils::{Logical, Point},
    wayland::socket::ListeningSocketSource,
};
use tracing_subscriber::EnvFilter;

use crate::{compositor::Meowland, tty::Terminal};

/// How often the compositor considers drawing a frame. Frame callbacks are what
/// pace clients, so this only bounds how long a client can be kept waiting for
/// its buffer to appear.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(flag) = arguments.first()
        && matches!(flag.as_str(), "-h" | "--help")
    {
        println!("{USAGE}");
        return Ok(());
    }
    if let Some(flag) = arguments.first()
        && matches!(flag.as_str(), "-V" | "--version")
    {
        println!("meowland {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let log = init_logging()?;
    let signal = install_signal_handler();

    let terminal = Terminal::new().context("could not take over the terminal")?;
    let capabilities = terminal.capabilities().clone();

    let socket = bind_socket()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    tracing::info!(socket = %socket_name, ?capabilities, "meowland starting");

    let display: Display<Meowland> =
        Display::new().context("could not create a Wayland display")?;
    let state = Meowland::new(&display.handle(), socket_name.clone(), &capabilities)?;

    let mut app = App {
        display,
        state,
        terminal,
        socket_name,
        command: arguments,
        children: Vec::new(),
        log,
        spawned: 0,
        quitting: false,
        signal: None,
    };

    let mut event_loop: Calloop<App> =
        Calloop::try_new().context("could not create an event loop")?;
    let signal_handle = event_loop.get_signal();
    app.signal = Some(signal_handle);

    let handle = event_loop.handle();

    handle
        .insert_source(socket, |stream, (), app| {
            if let Err(err) = app.state.insert_client(stream) {
                tracing::warn!(?err, "could not adopt a client");
            }
        })
        .context("could not watch the Wayland socket")?;

    let poll_fd = rustix::io::dup(app.display.backend().poll_fd())
        .context("could not take the display socket")?;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                // Clients have requests waiting: answer them, then push the
                // answers out.
                let dispatched = app.display.dispatch_clients(&mut app.state);
                if let Err(err) = dispatched {
                    tracing::warn!(?err, "dispatching to clients failed");
                }
                if let Err(err) = app.display.flush_clients() {
                    tracing::warn!(?err, "flushing to clients failed");
                }
                Ok(PostAction::Continue)
            },
        )
        .context("could not watch the display")?;

    let (sender, channel) = channel();
    handle
        .insert_source(channel, |event, (), app: &mut App| match event {
            ChannelEvent::Msg(TerminalEvent::Input(event)) => app.on_terminal_event(event),
            ChannelEvent::Msg(TerminalEvent::Closed) => {
                tracing::info!("the terminal went away");
                app.quit();
            }
            ChannelEvent::Closed => app.quit(),
        })
        .map_err(|err| anyhow::anyhow!("could not watch terminal input: {err:?}"))?;
    std::thread::spawn(move || read_terminal(&sender));

    handle
        .insert_source(
            Timer::from_duration(FRAME_INTERVAL),
            |_, (), app: &mut App| {
                app.tick();
                TimeoutAction::ToDuration(FRAME_INTERVAL)
            },
        )
        .map_err(|err| anyhow::anyhow!("could not install the frame timer: {err:?}"))?;

    if !app.command.is_empty() {
        app.spawn_client();
    }

    let result = event_loop.run(None, &mut app, |app| {
        if signal.load(Ordering::Relaxed) {
            app.quit();
        }
    });
    app.shutdown();
    result.context("the event loop failed")?;
    Ok(())
}

const USAGE: &str = "\
meowland: run one Wayland client in your terminal

Usage: meowland [COMMAND [ARGS...]]

  COMMAND    the client to run (foot, for instance). Without one, meowland waits for a
             client to connect to the socket it prints on startup.

The client fills the terminal: meowland draws no decorations and keeps no window list, and
its only key binding is

  Alt+Q      leave (the client goes with it).

Clients have to render into shared memory: meowland advertises wl_shm, xdg-shell, wl_seat,
wl_output and the cursor shape protocol, but nothing that would let a GPU client hand its
buffers over. Logs go to $XDG_RUNTIME_DIR/meowland.log (override with MEOWLAND_LOG).";

/// Everything the event loop owns.
struct App {
    /// Kept separate from `state`: dispatching needs both at once.
    display: Display<Meowland>,
    state: Meowland,
    terminal: Terminal,
    socket_name: String,
    /// The client command from the command line.
    command: Vec<String>,
    children: Vec<Child>,
    log: File,
    /// How many clients were started, so that "none left" can be told from
    /// "none yet".
    spawned: usize,
    quitting: bool,
    signal: Option<LoopSignal>,
}

impl App {
    /// A frame is due: draw whatever changed, and stop when the client is gone.
    fn tick(&mut self) {
        self.reap();
        if self.state.needs_frame()
            && let Err(err) = self.state.present(&mut self.terminal)
        {
            tracing::warn!(?err, "could not present a frame");
        }
        if let Err(err) = self.display.flush_clients() {
            tracing::warn!(?err, "flushing to clients failed");
        }
        if self.spawned > 0 && self.children.is_empty() {
            // The client meowland was started for is gone: there is nothing
            // left to show.
            tracing::info!("the client is gone");
            self.quit();
        }
        if self.state.quitting() {
            tracing::info!("the quit binding was used");
            self.quit();
        }
    }

    /// Run the client command, with the environment a Wayland client expects
    /// inside meowland.
    fn spawn_client(&mut self) {
        if self.command.is_empty() {
            tracing::info!("no command configured, nothing to spawn");
            return;
        }
        let (program, arguments) = self.command.split_first().expect("checked above");
        let log = match self.log.try_clone() {
            Ok(log) => log,
            Err(err) => {
                tracing::warn!(?err, "could not redirect the client's output");
                return;
            }
        };
        let child = Command::new(program)
            .args(arguments)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            // meowland is not an X11 server, so clients must not fall back to X.
            .env_remove("DISPLAY")
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("SDL_VIDEODRIVER", "wayland")
            .env("MOZ_ENABLE_WAYLAND", "1")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "auto")
            // The terminal belongs to the compositor: a client writing to it would draw over the
            // screen we are managing.
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("log file is clonable")))
            .stderr(Stdio::from(log))
            .spawn();
        match child {
            Ok(child) => {
                tracing::info!(program, pid = child.id(), "client started");
                self.children.push(child);
                self.spawned += 1;
            }
            Err(err) => tracing::warn!(?err, program, "could not start the client"),
        }
    }

    /// Collect children that exited.
    fn reap(&mut self) {
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(pid = child.id(), ?status, "client exited");
                false
            }
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(?err, "could not wait for a client");
                false
            }
        });
    }

    /// Handle one event from the terminal.
    fn on_terminal_event(&mut self, event: crossterm::event::Event) {
        use crossterm::event::{Event, MouseButton, MouseEventKind};
        match event {
            Event::Key(key) => self.state.key(key),
            Event::Mouse(mouse) => {
                let position: Point<f64, Logical> = if self.terminal.capabilities().pixel_mouse {
                    (f64::from(mouse.column), f64::from(mouse.row)).into()
                } else {
                    let (cell_width, cell_height) = self.terminal.capabilities().cell;
                    (
                        f64::mul_add(
                            f64::from(mouse.column),
                            f64::from(cell_width),
                            f64::from(cell_width) / 2.0,
                        ),
                        f64::mul_add(
                            f64::from(mouse.row),
                            f64::from(cell_height),
                            f64::from(cell_height) / 2.0,
                        ),
                    )
                        .into()
                };
                match mouse.kind {
                    MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                        self.state.pointer_motion(position);
                    }
                    MouseEventKind::Down(button) | MouseEventKind::Up(button) => {
                        let button = match button {
                            MouseButton::Left => keys::button::LEFT,
                            MouseButton::Right => keys::button::RIGHT,
                            MouseButton::Middle => keys::button::MIDDLE,
                        };
                        self.state.pointer_motion(position);
                        self.state
                            .pointer_button(button, matches!(mouse.kind, MouseEventKind::Down(_)));
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => {
                        self.state.pointer_axis(-15.0);
                    }
                    MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => {
                        self.state.pointer_axis(15.0);
                    }
                }
            }
            Event::Resize(_, _) => {
                let capabilities = self.terminal.refresh().clone();
                self.state.resize(&capabilities);
                // Whatever the terminal kept from before the resize is not ours
                // any more.
                self.terminal.clear();
            }
            Event::Paste(text) => self.state.paste(&text),
            Event::FocusGained | Event::FocusLost => {}
        }
    }

    /// Leave the event loop.
    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        if let Some(signal) = &self.signal {
            signal.stop();
        }
    }

    /// Restore the terminal and stop the clients.
    fn shutdown(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        tracing::info!("meowland stopped");
    }
}

/// Events the terminal input thread produces.
enum TerminalEvent {
    Input(crossterm::event::Event),
    Closed,
}

/// Read terminal events forever. crossterm parses the escape sequences,
/// including the kitty keyboard protocol extensions meowland asks for.
fn read_terminal(sender: &Sender<TerminalEvent>) {
    loop {
        match crossterm::event::read() {
            Ok(event) => {
                tracing::debug!(?event, "terminal input");
                if sender.send(TerminalEvent::Input(event)).is_err() {
                    return;
                }
            }
            Err(err) => {
                tracing::debug!(?err, "terminal input ended");
                let _ = sender.send(TerminalEvent::Closed);
                return;
            }
        }
    }
}

/// Bind the Wayland socket clients inside the compositor will use.
fn bind_socket() -> anyhow::Result<ListeningSocketSource> {
    // The well-known name if it is free, otherwise whatever number the display
    // picks next.
    ListeningSocketSource::with_name("wayland-meowland")
        .or_else(|_| ListeningSocketSource::new_auto())
        .context("could not bind a Wayland socket")
}

/// Send logs to a file: stdout is the screen we are drawing on.
fn init_logging() -> anyhow::Result<File> {
    let path = std::env::var_os("MEOWLAND_LOG").map_or_else(
        || {
            std::path::Path::new(
                &std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
            )
            .join("meowland.log")
        },
        std::path::PathBuf::from,
    );
    let file =
        File::create(&path).with_context(|| format!("could not log to {}", path.display()))?;
    let filter = EnvFilter::try_from_env("MEOWLAND_LOG_LEVEL")
        .unwrap_or_else(|_| EnvFilter::new("meowland=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer({
            let file = file.try_clone()?;
            move || file.try_clone().expect("log file is clonable")
        })
        .with_ansi(false)
        .init();
    tracing::info!(path = %path.display(), "logging to file");
    Ok(file)
}

/// Turn the signals a terminal sends into a flag the event loop polls, so the
/// terminal gets restored on the way out.
fn install_signal_handler() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        if let Err(err) = signal_hook::flag::register(signal, flag.clone()) {
            tracing::warn!(?err, signal, "could not install a signal handler");
        }
    }
    flag
}
