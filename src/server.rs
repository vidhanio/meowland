//! The server: the compositor, its windows and its clients, with no terminal.
//!
//! `run` starts one of these and `attach` shows it in a terminal that is
//! somewhere else entirely; what the two ends say to each other is
//! `src/display.rs`, and the commands that reach a server are
//! `src/control.rs`.
//!
//! Nothing here reads or writes a terminal. What the attached terminal says
//! arrives as messages from the thread reading its socket, what it is shown
//! goes back through the presenter, and a server with nobody attached simply
//! runs on: it accepts clients, keeps their state and draws nothing, which is
//! what makes attaching later a matter of showing what is already there.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use calloop::signals::{Signal, Signals};
use smithay::{
    reexports::{
        calloop::{
            EventLoop as Calloop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            RegistrationToken,
            channel::{Channel, Event as ChannelEvent, Sender, channel},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::Display,
    },
    wayland::socket::ListeningSocketSource,
};

use crate::{
    cli::Settings,
    compositor::{Cost, Meowland},
    control, display,
    display::{Input, ToClient, ToServer},
    logging,
    presenter::{Event as PresenterEvent, Presenter},
    tty::Capabilities,
    xwayland,
};

/// How often the compositor considers drawing a frame. Derived from the rate
/// clients are told about, so their pacing and ours cannot drift apart.
const FRAME_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000_000 / crate::compositor::REFRESH_MILLIHZ as u64);

/// How long a connection has to finish saying what it wants, which is long
/// enough for a request already in flight and short enough that a client which
/// says nothing does not hold anything up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// Run the server until something stops it.
///
/// A command here is what the server exists for: it is run in the server, and
/// the server stops once its clients are gone (`crate::cli::Server`).
pub fn run(settings: Settings, command: Vec<std::ffi::OsString>) -> anyhow::Result<()> {
    let Settings {
        gpu_buffers,
        render_node,
        log,
        log_level,
    } = settings;
    let log = logging::init(log.as_deref(), log_level.as_deref())?;

    // Both sockets are taken before anything else, so that a second server is
    // told one is running rather than being told about the terminal it is not
    // sitting in.
    let (control_socket, control_listener) = control::Socket::bind(control::CONTROL_SOCKET)?;
    let (display_socket, display_listener) = display::listen()?;

    let signals = Signals::new(&[
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])
    .context("could not listen for process signals")?;

    let socket = bind_socket()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    tracing::info!(socket = %socket_name, "meowland server starting");

    let display: Display<Meowland> =
        Display::new().context("could not create a Wayland display")?;
    let nodes = gpu_buffers.nodes(render_node.as_deref())?;
    // No terminal is attached yet, so the seat has nothing to tell clients
    // about: what a terminal can do arrives with the terminal.
    let state = Meowland::new(&display.handle(), &nodes)?;
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
    let (terminal_sender, terminal_events) = channel();
    // The loop comes first, because the app registers a pane's presenter events
    // with it as panes attach.
    let mut event_loop: Calloop<App> =
        Calloop::try_new().context("could not create an event loop")?;
    let handle = event_loop.handle();

    let mut app = App {
        display,
        state,
        xwayland,
        socket_name,
        log,
        panes: HashMap::new(),
        terminal_sender,
        readers: HashMap::new(),
        generations: 0,
        handle: handle.clone(),
        _sockets: (control_socket, display_socket),
        children: Vec::new(),
        quit_when_empty: !command.is_empty(),
        command,
        quitting: false,
        frame_scheduled: false,
        last_frame_started: None,
        frames: FrameStats::default(),
        signal: None,
    };

    app.signal = Some(event_loop.get_signal());
    install_sources(
        &handle,
        &mut app,
        Sources {
            clients: socket,
            terminals: display_listener,
            commands: control_listener,
            signals,
            said: terminal_events,
        },
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

/// Bind the Wayland socket clients inside the compositor will use.
fn bind_socket() -> anyhow::Result<ListeningSocketSource> {
    // The well-known name if it is free, otherwise whatever number the display
    // picks next.
    ListeningSocketSource::with_name("wayland-meowland")
        .or_else(|_| ListeningSocketSource::new_auto())
        .context("could not bind a Wayland socket")
}

/// Everything the event loop owns.
struct App {
    /// Kept separate from `state`: dispatching needs both at once.
    display: Display<Meowland>,
    state: Meowland,
    xwayland: Option<xwayland::Server>,
    socket_name: String,
    /// The log file, shared with every client this server starts: their own
    /// output cannot go to the terminal the server is drawn on.
    log: File,
    /// The panes attached at the moment, by the ID each was given.
    ///
    /// A pane is one terminal: it asks to be shown a window and is drawn that
    /// window, with a presenter of its own writing to its own socket. Nothing
    /// about one pane is another's business.
    panes: HashMap<u64, Pane>,
    /// What a pane's reader says comes through here, stamped with the pane it
    /// came from.
    terminal_sender: Sender<FromTerminal>,
    /// Readers of terminals that are connecting or attached, kept so that they
    /// can be let go of rather than left running.
    readers: HashMap<u64, JoinHandle<()>>,
    /// Handed out to each pane that connects, so that what one says can be told
    /// from what another says - and from what a pane that has gone said.
    generations: u64,
    /// Where sources are registered, and where a pane's are removed again when
    /// it goes.
    handle: LoopHandle<'static, Self>,
    /// Removes the sockets when the event loop ends.
    _sockets: (control::Socket, control::Socket),
    children: Vec<Child>,
    /// Whether to stop once the clients this server started are gone.
    ///
    /// A server started to run a command is a server that exists for it: the
    /// terminal that asked should come back when the command exits. One started
    /// to be a server - `meowland run` with no command, or a client connecting
    /// on its own - is one until the quit binding, `meowland quit` or a signal
    /// says otherwise, however many clients come and go.
    quit_when_empty: bool,
    /// The client command from the command line, if any.
    command: Vec<std::ffi::OsString>,
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

/// One terminal attached to this server.
///
/// The pane's own state - which window it shows, its geometry, the frame it is
/// drawn into - lives in the compositor, keyed by this ID (`Meowland::views`).
/// What is here is the socket, the thread writing frames to it, and the thread
/// reading what the user does at it.
#[derive(Debug)]
struct Pane {
    /// The pane's ID, which is what it is known by here and in the compositor.
    generation: u64,
    /// Kept to shut the socket down when the pane is let go, which is what ends
    /// its reader.
    stream: UnixStream,
    /// Writes this pane's frames, and compresses them, on a thread of its own.
    presenter: Presenter,
    /// The thread reading this pane's terminal, joined when it is let go.
    reader: Option<JoinHandle<()>>,
    /// The registration of this pane's presenter events, removed with it.
    drawn: RegistrationToken,
}

/// What a terminal that is connecting or attached says to a server.
#[derive(Debug)]
enum FromTerminal {
    /// A terminal said who it is and what it wants to be shown, which is
    /// answered by drawing on it or by saying why not.
    Hello {
        generation: u64,
        version: u32,
        show: display::Show,
        capabilities: Capabilities,
        /// The half of its socket the server writes to.
        stream: UnixStream,
    },
    /// Something the user did.
    Input { generation: u64, input: Input },
    /// The terminal was resized, and this is what it can do now.
    Resized {
        generation: u64,
        capabilities: Capabilities,
    },
    /// It has written the frame it was sent, so the next one may follow.
    Drawn { generation: u64 },
    /// It is on its way out, whether it said so or its socket ended.
    Left { generation: u64 },
}

impl FromTerminal {
    /// Which terminal said this.
    const fn generation(&self) -> u64 {
        match self {
            Self::Hello { generation, .. }
            | Self::Input { generation, .. }
            | Self::Resized { generation, .. }
            | Self::Drawn { generation }
            | Self::Left { generation } => *generation,
        }
    }
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
            // The server goes away with everything that was started under it,
            // which is what makes a server worth having: a server that
            // outlived its clients and could not be stopped would be one
            // nobody could get rid of.
            Some(control::Command::Quit) => {
                tracing::info!("the server was asked to stop");
                self.quit();
                control::Reply::Ok
            }
            None => control::Reply::Failed("unknown request".to_owned()),
        }
    }

    /// Draw a scheduled frame for every pane that has something to draw, and
    /// flush protocol replies.
    fn present_frame(&mut self) {
        let started = Instant::now();
        // The panes are drawn one at a time and each has a presenter of its
        // own: one terminal being slow holds up nothing but itself.
        let Self {
            state,
            panes,
            frames,
            ..
        } = self;
        for pane in panes.values_mut() {
            if !state.should_present_view(pane.generation, pane.presenter.is_ready()) {
                continue;
            }
            let cost = state.present_view(pane.generation, &mut pane.presenter);
            // The cap runs from when the frame began rather than when it
            // finished: the work happens *inside* the interval, so counting it
            // as well would put the compositor's own cost on the client's
            // latency and drop the frame rate with it.
            frames.record(&cost);
        }
        self.last_frame_started = Some(started);
        self.flush_clients();
    }

    /// Arm a frame if any pane has something to draw.
    fn schedule_frame(&mut self) {
        let handle = self.handle.clone();
        schedule_frame(&handle, self);
    }

    /// Whether any pane has something to draw and a presenter free to take it.
    fn panes_pending(&self) -> bool {
        self.panes.values().any(|pane| {
            self.state
                .should_present_view(pane.generation, pane.presenter.is_ready())
        })
    }

    /// Push queued protocol events to clients without waiting for client input.
    fn flush_clients(&mut self) {
        if let Err(err) = self.display.flush_clients() {
            tracing::warn!(?err, "flushing to clients failed");
        }
    }

    /// Take in a terminal that wants to show this server.
    ///
    /// What it says is read on a thread of its own: reading it here would be
    /// waiting for a terminal in the thread that draws frames for the one
    /// already attached.
    fn accept_terminal(&mut self, stream: UnixStream) {
        self.generations += 1;
        let generation = self.generations;
        let sender = self.terminal_sender.clone();
        let write_half = match stream.try_clone() {
            Ok(half) => half,
            Err(error) => {
                tracing::warn!(%error, "could not take a terminal's socket");
                return;
            }
        };
        match thread::Builder::new()
            .name("meowland-terminal".into())
            .spawn(move || read_terminal(stream, sender, generation, write_half))
        {
            Ok(reader) => {
                self.readers.insert(generation, reader);
            }
            Err(error) => tracing::warn!(%error, "could not start a terminal reader"),
        }
    }

    /// Handle what a terminal says.
    fn on_terminal(&mut self, message: FromTerminal) {
        // A pane that is not one of ours: it was turned away, or it has gone
        // and its reader had one more thing to say. A hello is how a pane
        // arrives, so it is the one message that can come from a pane that is
        // not here yet.
        let generation = message.generation();
        if !matches!(message, FromTerminal::Hello { .. }) && !self.has_pane(generation) {
            return;
        }
        match message {
            FromTerminal::Hello {
                generation,
                version,
                show,
                capabilities,
                stream,
            } => self.attach_pane(generation, version, show, &capabilities, stream),
            FromTerminal::Input { input, .. } => self.on_input(generation, input),
            FromTerminal::Resized { capabilities, .. } => {
                self.resize_pane(generation, &capabilities);
            }
            FromTerminal::Drawn { .. } => {
                if let Some(pane) = self.panes.get(&generation) {
                    pane.presenter.drawn();
                }
            }
            FromTerminal::Left { .. } => {
                tracing::info!(pane = generation, "the terminal went away");
                self.detach_pane(generation, None);
            }
        }
        if self.state.take_detach_request(generation) {
            tracing::info!(pane = generation, "the detach binding was used");
            self.detach_pane(generation, None);
        }
    }

    /// Whether this message came from a pane that is still attached.
    fn has_pane(&self, generation: u64) -> bool {
        self.panes.contains_key(&generation)
    }

    /// Answer a terminal that said hello: attach it, or say why not.
    ///
    /// Panes are independent, so nothing is refused for another pane's sake:
    /// as many terminals as want to can be attached at once, showing the same
    /// window or one each. The one thing that cannot be answered is a window ID
    /// no window has.
    fn attach_pane(
        &mut self,
        generation: u64,
        version: u32,
        show: display::Show,
        capabilities: &Capabilities,
        stream: UnixStream,
    ) {
        let refusal = if version != display::VERSION {
            Some(format!(
                "this server speaks version {} of the protocol, and this terminal speaks {version}",
                display::VERSION
            ))
        } else if let display::Show::Window(id) = show
            && !self.state.has_window(id)
        {
            Some("no window has that ID".to_owned())
        } else {
            None
        };
        if let Some(reason) = refusal {
            tracing::info!(%reason, "turned a terminal away");
            let mut stream = stream;
            let _ = display::write_to(
                &mut stream,
                display::encode_client(&ToClient::Detached(reason)),
            );
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }

        let Some(reader) = self.readers.remove(&generation) else {
            tracing::warn!("a terminal said hello that was not being read");
            return;
        };
        // The greeting is the first thing written on the socket, so it is said
        // here rather than handed to the presenter with everything else.
        let mut writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(%error, "could not take a terminal's socket");
                return;
            }
        };
        let _ = display::write_to(&mut writer, display::encode_client(&ToClient::Welcome));

        // Frames are written, and compressed, on a thread of this pane's own:
        // one terminal being slow must not hold up another.
        let draw_on = match stream.try_clone() {
            Ok(half) => half,
            Err(error) => {
                tracing::warn!(%error, "could not take a terminal's socket");
                return;
            }
        };
        let (drawn_sender, drawn) = channel();
        let presenter = match Presenter::new(drawn_sender) {
            Ok(presenter) => presenter,
            Err(error) => {
                tracing::warn!(%error, "could not start a presenter for a pane");
                return;
            }
        };
        presenter.attach(draw_on, capabilities.shared_memory);
        let drawn =
            match self
                .handle
                .clone()
                .insert_source(drawn, move |event, (), app: &mut Self| {
                    match event {
                        ChannelEvent::Msg(PresenterEvent::Ready(frame)) => {
                            if let Some(pane) = app.panes.get_mut(&generation) {
                                pane.presenter.recycle(frame);
                            }
                        }
                        // A presenter that has stopped is one pane that will not
                        // be drawn again; the others, and the windows, are not
                        // affected.
                        ChannelEvent::Msg(PresenterEvent::Failed(error)) => {
                            tracing::error!(%error, pane = generation, "presentation failed");
                        }
                        ChannelEvent::Closed => {}
                    }
                    app.schedule_frame();
                }) {
                Ok(token) => token,
                Err(error) => {
                    tracing::warn!(%error, "could not watch a pane's presenter");
                    return;
                }
            };

        self.state.attach_view(generation, show, capabilities);
        tracing::info!(pane = generation, ?capabilities, "pane attached");
        self.panes.insert(
            generation,
            Pane {
                generation,
                stream,
                presenter,
                reader: Some(reader),
                drawn,
            },
        );
    }

    /// Let a pane go.
    ///
    /// The other panes and the windows are untouched: a pane is a place to look
    /// at a window, and what it was looking at stays what it was.
    fn detach_pane(&mut self, generation: u64, reason: Option<&str>) {
        let Some(mut pane) = self.panes.remove(&generation) else {
            return;
        };
        if let Some(reason) = reason {
            pane.presenter.detach(ToClient::Detached(reason.to_owned()));
        }
        // Stop writing before the socket goes: what undoes the takeover has to
        // be the last thing this terminal is sent.
        pane.presenter.finish();
        self.handle.remove(pane.drawn);
        let _ = pane.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = pane.reader
            && reader.join().is_err()
        {
            tracing::error!("the pane's reader panicked");
        }
        self.state.detach_view(generation);
    }

    /// The terminal this pane is in changed size, or what it can do.
    fn resize_pane(&mut self, generation: u64, capabilities: &Capabilities) {
        let Some(pane) = self.panes.get(&generation) else {
            return;
        };
        // Whatever the terminal kept from before the resize is not this pane's
        // any more, and the wipe has to land after the frames already handed
        // over for the same reason.
        pane.presenter.clear();
        self.state.resize_view(generation, capabilities);
    }

    /// Handle something the user did at a pane.
    ///
    /// Everything is in the terms of the pane it happened in: the bindings act
    /// on the window that pane shows, typing gives that window the keyboard,
    /// and the pointer is somewhere in that pane's own geometry.
    fn on_input(&mut self, generation: u64, input: Input) {
        match input {
            Input::Key(key) => self.state.key(generation, key),
            Input::Pointer(pointer) => self.state.pointer(generation, pointer),
            Input::Paste(text) => self.state.paste(&text),
            Input::Focus(_) => {}
        }
    }

    /// Run the client the command line asked for, before the loop starts, so
    /// that a command that cannot start is reported to whoever started the
    /// server rather than leaving one with nothing to draw.
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
        Ok(())
    }

    /// Run a client, with the environment a Wayland client expects inside
    /// meowland and its output going where the server's own does.
    fn spawn_client(&mut self, command: &[std::ffi::OsString]) -> std::io::Result<()> {
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
            .stdout(self.client_output())
            .stderr(self.client_output());
        // meowland is not an X11 server. Without a satellite there is no X
        // display to give a client, and inheriting one would put its window on
        // a display outside this server.
        match &self.xwayland {
            Some(server) => child.env("DISPLAY", server.display()),
            None => child.env_remove("DISPLAY"),
        };
        let child = child.spawn()?;
        tracing::info!(program = %program.to_string_lossy(), pid = child.id(), "client started");
        self.children.push(child);
        Ok(())
    }

    /// Where a client's own output goes.
    ///
    /// Not the terminal. The server is drawn on it, so text written there
    /// lands in the cells the frame is placed on - and a newline among them
    /// scrolls the whole frame out from under itself. The log is where the
    /// server's own output goes, so a client that prints why it failed can
    /// still be read about afterwards.
    fn client_output(&self) -> Stdio {
        match self.log.try_clone() {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                tracing::warn!(%error, "could not send a client's output to the log");
                Stdio::null()
            }
        }
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

    /// Stop once the clients of a server started for a command are gone.
    fn check_quit(&mut self) {
        if !self.quitting && self.quit_when_empty && self.children.is_empty() {
            tracing::info!("the client is gone");
            self.quit();
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

    /// Let every pane go and stop the clients.
    fn shutdown(&mut self) {
        while let Some(generation) = self.panes.keys().copied().next() {
            self.detach_pane(generation, None);
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut xwayland) = self.xwayland.take() {
            xwayland.stop();
        }
        tracing::info!("meowland stopped");
    }
}

/// Read what one terminal says, until it stops saying it.
///
/// The terminal says who it is first, which is what the server answers by
/// drawing on it or by turning it away; everything after that is what the user
/// did, how the terminal changed, and how far the drawing has got.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the reader outlives whoever started it, so it owns its end of the channel rather than borrowing it"
)]
fn read_terminal(
    mut stream: UnixStream,
    sender: Sender<FromTerminal>,
    generation: u64,
    write_half: UnixStream,
) {
    // A terminal that connects and then says nothing is given up on rather
    // than waited for: the server has frames to draw for the one that is
    // attached.
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let hello = display::read_from(&mut stream);
    let _ = stream.set_read_timeout(None);
    let Some(ToServer::Hello {
        version,
        show,
        capabilities,
    }) = hello
        .ok()
        .flatten()
        .and_then(|(tag, payload)| display::decode(tag, &payload))
    else {
        return;
    };
    let hello = FromTerminal::Hello {
        generation,
        version,
        show,
        capabilities,
        stream: write_half,
    };
    if sender.send(hello).is_err() {
        return;
    }

    while let Ok(Some((tag, payload))) = display::read_from(&mut stream) {
        let Some(message) = display::decode(tag, &payload) else {
            continue;
        };
        let sent = match message {
            ToServer::Input(input) => sender.send(FromTerminal::Input { generation, input }),
            ToServer::Resized(capabilities) => sender.send(FromTerminal::Resized {
                generation,
                capabilities,
            }),
            ToServer::Drawn => sender.send(FromTerminal::Drawn { generation }),
            ToServer::Bye => sender.send(FromTerminal::Left { generation }),
            // Said once, at the start, and answered by then.
            ToServer::Hello { .. } => continue,
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = sender.send(FromTerminal::Left { generation });
}

/// Everything a server is reached through, once it is listening on all of it.
struct Sources {
    /// Clients connect here.
    clients: ListeningSocketSource,
    /// Terminals attach here.
    terminals: UnixListener,
    /// Commands arrive here.
    commands: UnixListener,
    /// Process signals arrive here.
    signals: Signals,
    /// What the attached terminals say arrives here. What their presenters do
    /// with the frames arrives on a source of its own, registered with the pane
    /// it belongs to (`App::attach_pane`).
    said: Channel<FromTerminal>,
}

fn install_sources(
    handle: &LoopHandle<'_, App>,
    app: &mut App,
    sources: Sources,
) -> anyhow::Result<()> {
    watch_clients(handle, sources.clients)?;
    watch_display(handle, &mut app.display)?;
    watch_terminal(handle, sources.said)?;
    watch_server(handle, sources.signals, sources.commands, sources.terminals)?;
    schedule_frame(handle, app);
    Ok(())
}

/// Watch the socket clients connect to.
fn watch_clients(
    handle: &LoopHandle<'_, App>,
    socket: ListeningSocketSource,
) -> anyhow::Result<()> {
    handle
        .insert_source(socket, |stream, (), app| {
            if let Err(err) = app.state.insert_client(stream) {
                tracing::warn!(?err, "could not adopt a client");
            }
        })
        .context("could not watch the Wayland socket")?;
    Ok(())
}

/// Watch the display: dispatching what clients sent, answering them, and
/// arming a frame when that leaves something to draw.
fn watch_display(
    handle: &LoopHandle<'_, App>,
    display: &mut Display<Meowland>,
) -> anyhow::Result<()> {
    let poll_fd = rustix::io::dup(display.backend().poll_fd())
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
    Ok(())
}

/// Watch what the attached terminals say.
fn watch_terminal(
    handle: &LoopHandle<'_, App>,
    terminal_events: Channel<FromTerminal>,
) -> anyhow::Result<()> {
    let terminal_loop = handle.clone();
    handle
        .insert_source(terminal_events, move |event, (), app: &mut App| {
            match event {
                ChannelEvent::Msg(message) => app.on_terminal(message),
                ChannelEvent::Closed => app.quit(),
            }
            app.flush_clients();
            app.check_quit();
            schedule_frame(&terminal_loop, app);
        })
        .map_err(|err| anyhow::anyhow!("could not watch the terminals: {err:?}"))?;
    Ok(())
}

/// Watch the things that reach a server: process signals, the socket commands
/// arrive on, and the socket terminals attach on.
fn watch_server(
    handle: &LoopHandle<'_, App>,
    signals: Signals,
    control_listener: UnixListener,
    display_listener: UnixListener,
) -> anyhow::Result<()> {
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
                            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                            let mut request = Vec::new();
                            if let Err(error) = stream.read_to_end(&mut request) {
                                tracing::warn!(%error, "could not read control request");
                                continue;
                            }
                            // A connection that said nothing wanted nothing: it
                            // is how "is a server there?" is asked, and the
                            // connection itself is the answer.
                            if request.is_empty() {
                                continue;
                            }
                            let reply = app.control_request(&request).encode();
                            if let Err(error) = stream.write_all(reply.as_bytes())
                                && error.kind() != std::io::ErrorKind::BrokenPipe
                            {
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

    handle
        .insert_source(
            Generic::new(display_listener, Interest::READ, Mode::Level),
            |_, listener, app: &mut App| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => app.accept_terminal(stream),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .context("could not watch the terminal socket")?;
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
    // Nobody is looking: a server with no pane attached keeps running, but
    // there is nothing to draw.
    if app.frame_scheduled || !app.panes_pending() {
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
    use std::time::{Duration, Instant};

    use super::{FRAME_INTERVAL, frame_deadline};

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
