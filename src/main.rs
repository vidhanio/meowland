//! meowland: a Wayland compositor that runs inside your terminal.
//!
//! Clients connect to a Wayland socket like they always do. Their windows are
//! rendered into a frame buffer the compositor owns, that frame buffer is
//! diffed against the previous frame, and the differences are sent to the
//! terminal as [kitty graphics protocol][spec] images, tiled into
//! the cell grid. Keyboard and mouse input goes the other way.
//!
//! One window is on screen at a time, and every window has an ID the server
//! gave it. The binary runs as that server, and as a client of it: `run` starts
//! a server or hands its command to the one already running, `list` and
//! `attach` reach it over a socket in `$XDG_RUNTIME_DIR` (`src/control.rs`),
//! and `attach` completes the IDs the running server has.
//!
//! [spec]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

mod buffer;
mod compositor;
mod control;
mod dmabuf;
mod gpu;
mod keys;
mod kitty;
mod presenter;
mod render;
mod tty;
mod xwayland;

use std::{
    ffi::OsString,
    fs::File,
    io::{Read as _, Write as _},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use calloop::signals::{Signal, Signals};
use smithay::{
    reexports::{
        calloop::{
            EventLoop as Calloop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent, Sender, channel},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::Display,
    },
    utils::{Logical, Point},
    wayland::socket::ListeningSocketSource,
};
use tracing_subscriber::EnvFilter;

use crate::{
    compositor::{Cost, Meowland},
    presenter::{Event as PresenterEvent, Presenter},
    tty::Terminal,
};

/// The refresh rate advertised to clients, in the Wayland protocol's mHz.
const REFRESH_MILLIHZ: i32 = 60_000;

/// How often the compositor considers drawing a frame. Derived from the same
/// rate clients see, so their pacing and ours cannot drift apart.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000_000 / REFRESH_MILLIHZ as u64);

/// How long a control connection has to finish saying what it wants, which is
/// long enough for a request already in flight and short enough that a client
/// which says nothing does not hold up frames.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.action {
        Action::Run(parsed) => run(parsed),
        Action::Attach(attach) => attach_window(attach.id),
        Action::List(_) => list_windows(),
        Action::Completions(completions) => {
            print!("{}", Cli::completion_script(completions.shell.into()));
            Ok(())
        }
    }
}

/// `meowland run`: be the server, or hand the command to one already running.
///
/// A server owns the terminal it was started in and the windows in it, so a
/// command typed anywhere else is a request to that server: it starts the
/// client and answers, and the CLI that asked exits. With no command there is
/// nothing to hand over, and the only thing `run` can mean is "be a server" -
/// which is an error when one already is.
fn run(run: Run) -> anyhow::Result<()> {
    if !run.command.is_empty() {
        match control::request(&control::Command::Run(run.command.clone())) {
            Ok(reply) => return accepted(reply),
            Err(control::Error::NotRunning) => {}
            Err(error) => return Err(error.into()),
        }
    }
    run_server(run)
}

fn run_server(run: Run) -> anyhow::Result<()> {
    let Run {
        gpu_buffers,
        render_node,
        log,
        log_level,
        command,
    } = run;
    init_logging(log.as_deref(), log_level.as_deref())?;

    // Taken before anything else, so that a second `run` is told a server is
    // already running rather than being told about the terminal it is not
    // sitting in.
    let (control, control_listener) = control::Socket::bind()?;

    let terminal = Terminal::new()?;
    // Block process signals before starting any worker threads so every thread
    // inherits the mask and the event-loop signalfd receives them reliably.
    let signals = Signals::new(&[
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])
    .context("could not listen for process signals")?;

    let socket = bind_socket()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    tracing::info!(socket = %socket_name, "meowland starting");

    let display: Display<Meowland> =
        Display::new().context("could not create a Wayland display")?;
    let nodes = gpu_buffers.nodes(render_node.as_deref())?;
    let state = Meowland::new(&display.handle(), terminal.capabilities(), &nodes)?;
    // X11 clients are the satellite's business, not the compositor's: without
    // it a Wayland client still gets a window, so this is a warning and not a
    // reason to refuse to start.
    let xwayland = match xwayland::Server::start(&socket_name) {
        Ok(server) => Some(server),
        Err(error) => {
            tracing::warn!(%error, "X11 clients will not work: xwayland-satellite did not start");
            None
        }
    };
    let (presenter_sender, presenter_events) = channel();
    let presenter = Presenter::new(terminal, presenter_sender)?;
    let (terminal_sender, terminal_events) = channel();

    let mut app = App {
        display,
        state,
        terminal_sender: Some(terminal_sender),
        terminal_input: None,
        presenter,
        xwayland,
        socket_name,
        quit_when_empty: !command.is_empty(),
        command,
        _control: control,
        children: Vec::new(),
        quitting: false,
        frame_scheduled: false,
        last_frame_started: None,
        frames: FrameStats::default(),
        signal: None,
    };

    let mut event_loop: Calloop<App> =
        Calloop::try_new().context("could not create an event loop")?;
    let signal_handle = event_loop.get_signal();
    app.signal = Some(signal_handle);

    let handle = event_loop.handle();
    install_sources(
        &handle,
        &mut app,
        socket,
        terminal_events,
        presenter_events,
        signals,
        control_listener,
    )?;

    let result = app.start_client().and_then(|()| {
        if app.quitting {
            return Ok(());
        }
        event_loop
            .run(None, &mut app, |_| {})
            .context("the event loop failed")
    });
    app.shutdown();
    result
}

/// Whether a server carried a command out.
fn accepted(reply: control::Reply) -> anyhow::Result<()> {
    match reply {
        control::Reply::Ok => Ok(()),
        control::Reply::Failed(reason) => anyhow::bail!(reason),
        // Only `list` is answered with windows, and this is not `list`.
        control::Reply::Windows(_) => anyhow::bail!("the server answered with a window list"),
    }
}

/// `meowland attach <id>`: make one open window the one on screen.
fn attach_window(id: u64) -> anyhow::Result<()> {
    accepted(control::request(&control::Command::Attach(id))?)
}

/// `meowland list`: the open windows, which are what `attach` takes.
fn list_windows() -> anyhow::Result<()> {
    let control::Reply::Windows(windows) = control::request(&control::Command::List)? else {
        anyhow::bail!("the server did not answer with a window list");
    };
    for window in windows {
        let mut fields = vec![window.id.to_string()];
        if !window.label.is_empty() {
            fields.push(window.label);
        }
        if window.active {
            fields.push("active".to_owned());
        }
        println!("{}", fields.join("\t"));
    }
    Ok(())
}

/// The window IDs `attach` may be given, read from the running server.
///
/// A shell asks this while the user is typing, so a server that is not running
/// is an empty answer and not an error: there is nothing to complete, and a
/// complaint printed into someone's half-finished command line would be worse
/// than offering nothing.
fn attached_windows(
    _partial: &<Attach as usage::argv::spec::CommandArgs>::Partial,
    _ctx: &usage::complete::CompleteCtx<'_>,
) -> Vec<usage::complete::Candidate<'static>> {
    let Ok(control::Reply::Windows(windows)) = control::request(&control::Command::List) else {
        return Vec::new();
    };
    windows
        .into_iter()
        .map(|window| {
            let mut description = window.label;
            if window.active {
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str("(active)");
            }
            let id = window.id.to_string();
            if description.is_empty() {
                usage::complete::Candidate::new(id)
            } else {
                usage::complete::Candidate::described(id, description)
            }
        })
        .collect()
}

fn install_sources(
    handle: &LoopHandle<'_, App>,
    app: &mut App,
    socket: ListeningSocketSource,
    terminal_events: Channel<TerminalEvent>,
    presenter_events: Channel<PresenterEvent>,
    signals: Signals,
    control_listener: UnixListener,
) -> anyhow::Result<()> {
    handle
        .insert_source(socket, |stream, (), app| {
            if let Err(err) = app.state.insert_client(stream) {
                tracing::warn!(?err, "could not adopt a client");
            }
        })
        .context("could not watch the Wayland socket")?;

    let poll_fd = rustix::io::dup(app.display.backend().poll_fd())
        .context("could not take the display socket")?;
    let display_loop = handle.clone();
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, Mode::Level),
            move |_, _, app: &mut App| {
                let dispatched = app.display.dispatch_clients(&mut app.state);
                if let Err(err) = dispatched {
                    tracing::warn!(?err, "dispatching to clients failed");
                }
                app.flush_clients();
                app.check_quit();
                schedule_frame(&display_loop, app);
                Ok(PostAction::Continue)
            },
        )
        .context("could not watch the display")?;

    let terminal_loop = handle.clone();
    handle
        .insert_source(terminal_events, move |event, (), app: &mut App| {
            match event {
                ChannelEvent::Msg(TerminalEvent::Input(event)) => app.on_terminal_event(event),
                ChannelEvent::Msg(TerminalEvent::Closed) => {
                    tracing::info!("the terminal went away");
                    app.quit();
                }
                ChannelEvent::Closed => app.quit(),
            }
            app.flush_clients();
            app.check_quit();
            schedule_frame(&terminal_loop, app);
        })
        .map_err(|err| anyhow::anyhow!("could not watch terminal input: {err:?}"))?;

    let presenter_loop = handle.clone();
    handle
        .insert_source(presenter_events, move |event, (), app: &mut App| {
            match event {
                ChannelEvent::Msg(PresenterEvent::Ready(frame)) => app.presenter.recycle(frame),
                ChannelEvent::Msg(PresenterEvent::Failed(error)) => {
                    tracing::error!(%error, "terminal presentation failed");
                    app.quit();
                }
                ChannelEvent::Closed => app.quit(),
            }
            app.check_quit();
            schedule_frame(&presenter_loop, app);
        })
        .map_err(|err| anyhow::anyhow!("could not watch terminal presentation: {err:?}"))?;

    handle
        .insert_source(signals, |event, (), app: &mut App| match event.signal() {
            Signal::SIGCHLD => {
                app.reap();
                app.check_quit();
            }
            _ => app.quit(),
        })
        .context("could not watch process signals")?;

    handle
        .insert_source(
            Generic::new(control_listener, Interest::READ, Mode::Level),
            |_, listener, app: &mut App| {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // The event loop is drawing frames, so a client
                            // that connects and then says nothing is given up
                            // on rather than waited for.
                            let _ = stream.set_read_timeout(Some(CONTROL_TIMEOUT));
                            let mut request = Vec::new();
                            if let Err(error) = stream.read_to_end(&mut request) {
                                tracing::warn!(%error, "could not read control request");
                                continue;
                            }
                            let reply = app.control_request(&request).encode();
                            if let Err(error) = stream.write_all(reply.as_bytes()) {
                                tracing::warn!(%error, "could not answer control request");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .context("could not watch the control socket")?;

    schedule_frame(handle, app);
    Ok(())
}

/// A terminal-native Wayland compositor.
#[derive(usage::Cli)]
#[usage(
    bin = "meowland",
    version = env!("CARGO_PKG_VERSION"),
    arg_required_else_help,
    completion,
    after_help = "Alt+Tab cycles the open windows and Alt+Q stops the server.\n\nA server owns the terminal it was started in. `run` starts one, or hands its command to the one already running; `list` and `attach` reach that server from anywhere else."
)]
struct Cli {
    #[usage(subcommand)]
    action: Action,
}

#[derive(usage::Subcommands)]
enum Action {
    /// Start the server, or run a client in the one that is running
    Run(Run),
    /// Show one open window, by its server ID
    Attach(Attach),
    /// Print the open windows and the IDs `attach` takes
    List(List),
    /// Print a shell completion script
    Completions(Completions),
}

#[derive(usage::Args)]
#[usage(
    after_help = "Every setting has a command line flag and an environment variable; the flag wins."
)]
struct Run {
    /// How clients are offered GPU buffers
    #[usage(
        long,
        value_enum,
        value_name = "WHEN",
        env = "MEOWLAND_GPU_BUFFERS",
        default = "auto"
    )]
    gpu_buffers: dmabuf::Offer,

    /// The node clients are told to render on, instead of the first one a
    /// renderer can be built on
    #[usage(long, env = "MEOWLAND_RENDER_NODE", value_name = "PATH")]
    render_node: Option<PathBuf>,

    /// Where to write logs, instead of `$XDG_RUNTIME_DIR/meowland.log`
    #[usage(long, env = "MEOWLAND_LOG", value_name = "PATH")]
    log: Option<PathBuf>,

    /// A `tracing` filter, as in `meowland=debug`
    #[usage(long, env = "MEOWLAND_LOG_LEVEL", value_name = "FILTER")]
    log_level: Option<String>,

    /// Client command and arguments. Without one, wait for a client to connect.
    #[usage(
        value_name = "COMMAND",
        value_hint = usage::ValueHint::CommandWithArguments,
        double_dash = "automatic"
    )]
    command: Vec<OsString>,
}

#[derive(usage::Args)]
struct Attach {
    /// Server-assigned window ID
    #[usage(value_name = "ID", complete = attached_windows)]
    id: u64,
}

#[derive(usage::Args)]
struct List;

#[derive(usage::Args)]
struct Completions {
    /// Shell to print the script for
    #[usage(value_enum, value_name = "SHELL")]
    shell: CompletionShell,
}

/// The shells a completion script is printed for.
///
/// The script is the one `usage` generates for this CLI, which calls back into
/// `meowland __complete_word__` - so `attach` completes the IDs of the windows
/// the running server actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    #[usage(name = "nu")]
    Nushell,
    #[usage(name = "powershell")]
    PowerShell,
}

impl From<CompletionShell> for usage::complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Nushell => Self::Nu,
            CompletionShell::PowerShell => Self::PowerShell,
        }
    }
}

/// Everything the event loop owns.
struct App {
    /// Kept separate from `state`: dispatching needs both at once.
    display: Display<Meowland>,
    state: Meowland,
    /// Held until a window appears; before then stdin remains untouched.
    terminal_sender: Option<Sender<TerminalEvent>>,
    /// Declared before the presenter so unwinding stops input before restoring
    /// the terminal, just as normal shutdown does.
    terminal_input: Option<TerminalInput>,
    presenter: Presenter,
    xwayland: Option<xwayland::Server>,
    socket_name: String,
    /// Whether to leave once the clients it started are gone.
    ///
    /// A server started to run a command gives the terminal back when that
    /// command is done. One started to be a server is one until the quit
    /// binding or the terminal says otherwise, however many clients come
    /// and go.
    quit_when_empty: bool,
    /// The client command from the command line, if any.
    command: Vec<OsString>,
    /// Removes the local server socket when the event loop ends.
    _control: control::Socket,
    children: Vec<Child>,
    quitting: bool,
    signal: Option<LoopSignal>,
    /// Whether a one-shot frame timer is already armed.
    frame_scheduled: bool,
    /// When the last successfully presented frame *began*, which is the clock
    /// the frame cap runs from.
    last_frame_started: Option<Instant>,
    /// What the frames of the last second cost, so that a slow frame rate can
    /// be told apart from a slow terminal.
    frames: FrameStats,
}

impl App {
    /// Answer one request from the control socket.
    fn control_request(&mut self, request: &[u8]) -> control::Reply {
        match control::Command::decode(request) {
            Some(control::Command::List) => control::Reply::Windows(
                self.state
                    .windows()
                    .map(|window| control::Window {
                        id: window.id,
                        label: window.label,
                        active: window.active,
                    })
                    .collect(),
            ),
            Some(control::Command::Attach(id)) => {
                if self.state.activate(id) {
                    control::Reply::Ok
                } else {
                    control::Reply::Failed("no window has that ID".to_owned())
                }
            }
            Some(control::Command::Run(argv)) => {
                let program = argv.first().map_or_else(
                    || "the client".to_owned(),
                    |program| program.to_string_lossy().into_owned(),
                );
                match self.spawn_client(&argv) {
                    Ok(()) => control::Reply::Ok,
                    Err(error) => {
                        control::Reply::Failed(format!("could not start {program}: {error}"))
                    }
                }
            }
            None => control::Reply::Failed("unknown request".to_owned()),
        }
    }

    /// Whether the terminal is ours to draw on, taking it over the first time a
    /// window has pixels to draw.
    ///
    /// Taking it over before that would wipe the screen for a frame of backdrop
    /// and hand it straight back, which is what a client that is still starting
    /// up looks like from here.
    fn take_terminal(&mut self) -> bool {
        self.terminal_input.is_some() || (self.state.window_ready() && self.activate_terminal())
    }

    /// Enter compositor terminal mode after the first window has pixels.
    fn activate_terminal(&mut self) -> bool {
        if self.terminal_input.is_some() {
            return true;
        }
        let capabilities = match self.presenter.activate() {
            Ok(capabilities) => capabilities,
            Err(error) => {
                tracing::error!(%error, "could not take over the terminal");
                self.quit();
                return false;
            }
        };
        tracing::info!(?capabilities, "terminal activated");
        self.state.resize(&capabilities);

        let Some(sender) = self.terminal_sender.take() else {
            self.quit();
            return false;
        };
        match TerminalInput::start(sender) {
            Ok(input) => {
                self.terminal_input = Some(input);
                true
            }
            Err(error) => {
                tracing::error!(%error, "could not start the terminal input thread");
                self.quit();
                false
            }
        }
    }

    /// Draw a scheduled frame and flush protocol replies.
    fn present_frame(&mut self) {
        let started = Instant::now();
        let cost = self.state.present(&mut self.presenter);
        // The cap runs from when the frame began rather than when it
        // finished: the work happens *inside* the interval, so counting it
        // as well would put the compositor's own cost on the client's
        // latency and drop the frame rate with it.
        self.last_frame_started = Some(started);
        self.frames.record(&cost);
        self.flush_clients();
    }

    /// Push queued protocol events to clients without waiting for client input.
    fn flush_clients(&mut self) {
        if let Err(err) = self.display.flush_clients() {
            tracing::warn!(?err, "flushing to clients failed");
        }
    }

    /// Stop once the clients of a one-command server are gone, or the quit
    /// binding says to.
    fn check_quit(&mut self) {
        if self.quitting {
            return;
        }
        if self.quit_when_empty && self.children.is_empty() {
            tracing::info!("the client is gone");
            self.quit();
        } else if self.state.quitting() {
            tracing::info!("the quit binding was used");
            self.quit();
        }
    }

    /// Run the client the command line asked for, before the loop starts, so
    /// that a command that cannot start is reported to the shell that typed it
    /// rather than leaving a server with nothing to draw.
    fn start_client(&mut self) -> anyhow::Result<()> {
        let Some((program, _)) = self.command.split_first() else {
            return Ok(());
        };
        let program = program.to_string_lossy().into_owned();
        let command = self.command.clone();
        if let Err(error) = self.spawn_client(&command) {
            anyhow::bail!("could not start {program}: {error}");
        }
        // Close the spawn-to-signalfd race for a client that exited
        // immediately.
        self.reap();
        self.check_quit();
        Ok(())
    }

    /// Run a client, with the environment a Wayland client expects inside
    /// meowland.
    fn spawn_client(&mut self, command: &[OsString]) -> std::io::Result<()> {
        let Some((program, arguments)) = command.split_first() else {
            return Ok(());
        };
        let mut child = Command::new(program);
        child
            .args(arguments)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("SDL_VIDEODRIVER", "wayland")
            .env("MOZ_ENABLE_WAYLAND", "1")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "auto")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // meowland is not an X11 server. Without a satellite there is no X
        // display to give a client, and inheriting one would put its window on
        // a display outside this terminal.
        match &self.xwayland {
            Some(server) => child.env("DISPLAY", server.display()),
            None => child.env_remove("DISPLAY"),
        };
        let child = child.spawn()?;
        tracing::info!(program = %program.to_string_lossy(), pid = child.id(), "client started");
        self.children.push(child);
        Ok(())
    }

    /// Collect children that exited.
    fn reap(&mut self) {
        let satellite_exited =
            self.xwayland
                .as_mut()
                .is_some_and(|server| match server.try_wait() {
                    Ok(Some(status)) => {
                        tracing::warn!(?status, "xwayland-satellite exited");
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        tracing::warn!(%error, "could not wait for xwayland-satellite");
                        true
                    }
                });
        if satellite_exited {
            self.xwayland = None;
        }
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
                let position: Point<f64, Logical> = if self.presenter.capabilities().pixel_mouse {
                    (f64::from(mouse.column), f64::from(mouse.row)).into()
                } else {
                    let (cell_width, cell_height) = self.presenter.capabilities().cell;
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
                let capabilities = self.presenter.refresh().clone();
                self.state.resize(&capabilities);
                // Whatever the terminal kept from before the resize is not ours
                // any more, and this has to land after the frames already
                // handed over for the same reason.
                self.presenter.clear();
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
        if let Some(mut input) = self.terminal_input.take() {
            input.stop();
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut xwayland) = self.xwayland.take() {
            xwayland.stop();
        }
        // Stop writing before the terminal is dropped: what undoes the takeover
        // has to be the last thing it is sent.
        self.presenter.finish();
        tracing::info!("meowland stopped");
    }
}

/// Events the terminal input thread produces.
enum TerminalEvent {
    Input(crossterm::event::Event),
    Closed,
}

/// The blocking terminal reader and its shutdown signal.
struct TerminalInput {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TerminalInput {
    fn start(sender: Sender<TerminalEvent>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("meowland-input".into())
            .spawn(move || read_terminal(&sender, &reader_stop))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!("the terminal input thread panicked");
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read terminal events forever. crossterm parses the escape sequences,
/// including the kitty keyboard protocol extensions meowland asks for.
fn read_terminal(sender: &Sender<TerminalEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                tracing::debug!(?err, "terminal input ended");
                let _ = sender.send(TerminalEvent::Closed);
                return;
            }
        }
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
fn init_logging(path: Option<&Path>, level: Option<&str>) -> anyhow::Result<()> {
    let path = path.map_or_else(
        || {
            Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
                .join("meowland.log")
        },
        Path::to_path_buf,
    );
    let file =
        File::create(&path).with_context(|| format!("could not log to {}", path.display()))?;
    let filter = level.map_or_else(
        || EnvFilter::new("meowland=info,warn"),
        |level| EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("meowland=info,warn")),
    );
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .init();
    tracing::info!(path = %path.display(), "logging to file");
    Ok(())
}

/// What the frames of one second cost this thread, logged so that a slow frame
/// rate can be attributed rather than guessed at: this is the thread that reads
/// input, so time spent here is time a keystroke waits.
///
/// The presenter keeps its own count, of the compressing and the writing.
#[derive(Debug, Default)]
struct FrameStats {
    since: Option<Instant>,
    frames: u32,
    tiles: u64,
    sent: u64,
    compose: Duration,
}

impl FrameStats {
    fn record(&mut self, cost: &Cost) {
        self.frames += 1;
        self.tiles += cost.tiles as u64;
        self.sent += cost.sent as u64;
        self.compose += cost.compose;

        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        let elapsed = now - since;
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let frames = f64::from(self.frames);
        tracing::debug!(
            fps = frames / elapsed.as_secs_f64(),
            tiles = self.tiles / u64::from(self.frames),
            sent = self.sent / u64::from(self.frames),
            compose_ms = self.compose.as_secs_f64() * 1e3 / frames,
            "frames composed"
        );
        self.since = Some(now);
        self.frames = 0;
        self.tiles = 0;
        self.sent = 0;
        self.compose = Duration::ZERO;
    }
}

/// The earliest deadline that preserves the frame cap without adding idle
/// latency.
///
/// `last_frame_started` is when the previous frame *began*: a frame that takes
/// longer than the interval is late, but the next one is not pushed out by the
/// work it already paid for.
fn frame_deadline(now: Instant, last_frame_started: Option<Instant>) -> Instant {
    last_frame_started
        .and_then(|started| started.checked_add(FRAME_INTERVAL))
        .map_or(now, |deadline| deadline.max(now))
}

/// Arm one frame deadline when new compositor state needs presentation.
fn schedule_frame(handle: &LoopHandle<'_, App>, app: &mut App) {
    if app.quitting {
        return;
    }
    if !app.take_terminal() {
        return;
    }
    if app.frame_scheduled || !app.state.should_present(app.presenter.is_ready()) {
        return;
    }
    app.frame_scheduled = true;
    let next_loop = handle.clone();
    let deadline = frame_deadline(Instant::now(), app.last_frame_started);
    if let Err(err) = handle.insert_source(
        Timer::from_deadline(deadline),
        move |_, (), app: &mut App| {
            app.frame_scheduled = false;
            app.present_frame();
            app.check_quit();
            schedule_frame(&next_loop, app);
            TimeoutAction::Drop
        },
    ) {
        app.frame_scheduled = false;
        tracing::warn!(?err, "could not schedule a frame");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        path::PathBuf,
        time::{Duration, Instant},
    };

    use super::{Action, Cli, FRAME_INTERVAL, frame_deadline};

    #[test]
    fn settings_are_flags_before_the_command() {
        let parsed = Cli::parse_from(
            [
                "run",
                "--gpu-buffers",
                "off",
                "--render-node",
                "/dev/dri/renderD129",
                "foot",
                "-T",
                "meowland",
            ]
            .map(OsStr::new)
            .as_slice(),
        )
        .expect("settings and a client command should parse");

        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(parsed.gpu_buffers, crate::dmabuf::Offer::Off);
        assert_eq!(
            parsed.render_node,
            Some(PathBuf::from("/dev/dri/renderD129"))
        );
        // The client's own flags are not the compositor's to read.
        assert_eq!(
            parsed.command,
            ["foot", "-T", "meowland"].map(OsString::from)
        );
    }

    #[test]
    fn client_flags_are_forwarded_after_the_command() {
        let parsed = Cli::parse_from(
            ["run", "foot", "--server", "-T", "meowland"]
                .map(OsStr::new)
                .as_slice(),
        )
        .expect("client arguments should parse");

        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(
            parsed.command,
            ["foot", "--server", "-T", "meowland"].map(OsString::from)
        );
    }

    #[test]
    fn an_idle_compositor_schedules_the_next_frame_immediately() {
        let now = Instant::now();
        let old = now
            .checked_sub(FRAME_INTERVAL + Duration::from_millis(1))
            .expect("the monotonic clock has advanced past startup");
        assert_eq!(frame_deadline(now, None), now);
        assert_eq!(frame_deadline(now, Some(old)), now);
    }

    #[test]
    fn an_active_compositor_preserves_the_frame_cap() {
        let started = Instant::now();
        let now = started + Duration::from_millis(1);
        assert_eq!(frame_deadline(now, Some(started)), started + FRAME_INTERVAL);
    }

    #[test]
    fn a_slow_frame_does_not_push_the_next_one_out() {
        // A frame that took longer than the interval has already paid for its
        // own time: the next one goes as soon as there is something to show,
        // rather than a further interval after the work finished.
        let now = Instant::now();
        let started = now
            .checked_sub(FRAME_INTERVAL + Duration::from_millis(10))
            .expect("the monotonic clock has advanced past startup");
        assert_eq!(frame_deadline(now, Some(started)), now);
    }
}
