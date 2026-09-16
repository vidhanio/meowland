//! Server event loop and pane lifecycle.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

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
    Error,
    cli::Settings,
    compositor::{Cost, Meowland},
    control, display,
    display::{Input, ToClient, ToServer},
    logging,
    presenter::{Event as PresenterEvent, Presenter},
    process,
    tty::Capabilities,
    types::{PaneId, ProcessId, ProtocolVersion},
    xwayland,
};

const FRAME_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000_000 / crate::compositor::REFRESH_MILLIHZ as u64);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

pub fn run(settings: Settings) -> Result<(), Error> {
    let Settings {
        gpu_buffers,
        render_node,
        log,
        log_level,
    } = settings;
    let log = logging::init(log.as_deref(), log_level.as_deref())?;

    let (control_socket, control_listener) = control::Socket::bind(control::CONTROL_SOCKET)?;
    let (display_socket, display_listener) = display::listen()?;

    let signals = Signals::new(&[
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])?;

    let socket = bind_socket()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    tracing::info!(socket = %socket_name, "meowland server starting");

    let display: Display<Meowland> = Display::new().map_err(Error::Display)?;
    let nodes = gpu_buffers.nodes(render_node.as_deref())?;
    let state = Meowland::new(&display.handle(), &nodes)?;
    let xwayland = match xwayland::Server::start(&socket_name) {
        Ok(server) => Some(server),
        Err(error) => {
            tracing::warn!(%error, "X11 clients will not work: xwayland-satellite did not start");
            None
        }
    };
    let (terminal_sender, terminal_events) = channel();
    let mut event_loop: Calloop<App> = Calloop::try_new()?;
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
        next_pane: PaneId::new(0),
        handle: handle.clone(),
        _sockets: (control_socket, display_socket),
        children: Vec::new(),
        stopping: false,
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

    let result = event_loop.run(None, &mut app, |_| {}).map_err(Error::from);
    app.shutdown();
    result
}

fn bind_socket() -> Result<ListeningSocketSource, Error> {
    ListeningSocketSource::with_name("wayland-meowland")
        .or_else(|_| ListeningSocketSource::new_auto())
        .map_err(Error::WaylandSocket)
}

struct App {
    /// Kept apart from `state`, because dispatching needs both at once.
    display: Display<Meowland>,
    state: Meowland,
    xwayland: Option<xwayland::Server>,
    socket_name: String,
    log: File,
    panes: HashMap<PaneId, Pane>,
    terminal_sender: Sender<FromTerminal>,
    readers: HashMap<PaneId, JoinHandle<()>>,
    next_pane: PaneId,
    handle: LoopHandle<'static, Self>,
    /// Removes the sockets when the event loop ends.
    _sockets: (control::Socket, control::Socket),
    children: Vec<Child>,
    stopping: bool,
    signal: Option<LoopSignal>,
    frame_scheduled: bool,
    last_frame_started: Option<Instant>,
    frames: FrameStats,
}

#[derive(Debug)]
struct Pane {
    stream: UnixStream,
    presenter: Presenter,
    reader: Option<JoinHandle<()>>,
    drawn: RegistrationToken,
}

#[derive(Debug)]
enum FromTerminal {
    Hello {
        pane: PaneId,
        version: ProtocolVersion,
        show: display::Show,
        capabilities: Capabilities,
        stream: UnixStream,
    },
    Input {
        pane: PaneId,
        input: Input,
    },
    Resized {
        pane: PaneId,
        capabilities: Capabilities,
    },
    Drawn {
        pane: PaneId,
    },
    Left {
        pane: PaneId,
    },
}

impl FromTerminal {
    const fn pane(&self) -> PaneId {
        match self {
            Self::Hello { pane, .. }
            | Self::Input { pane, .. }
            | Self::Resized { pane, .. }
            | Self::Drawn { pane }
            | Self::Left { pane } => *pane,
        }
    }
}

impl App {
    fn control_request(&mut self, request: &[u8]) -> control::Reply {
        match control::Command::decode(request) {
            Some(control::Command::List) => control::Reply::Windows(self.state.windows().collect()),
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
            // The whole server stops, and every client that it started goes
            // with it.
            Some(control::Command::Stop) => {
                tracing::info!("the server was asked to stop");
                self.stop();
                control::Reply::Ok
            }
            None => control::Reply::Failed("unknown request".to_owned()),
        }
    }

    /// Draw a frame for every pane that has something to draw, then flush
    /// replies.
    fn present_frame(&mut self) {
        let started = Instant::now();
        // Each pane has a presenter of its own, so a slow terminal holds up
        // only itself.
        let Self {
            state,
            panes,
            frames,
            ..
        } = self;
        for (pane, attached) in panes.iter_mut() {
            if !state.should_present_view(*pane, attached.presenter.is_ready()) {
                continue;
            }
            let cost = state.present_view(*pane, &mut attached.presenter);
            frames.record(&cost);
        }
        self.last_frame_started = Some(started);
        self.flush_clients();
    }

    fn settled(&mut self) {
        self.end_closed_panes();
        self.flush_clients();
        self.schedule_frame();
    }

    fn end_closed_panes(&mut self) {
        for pane in self.state.take_closed_views() {
            tracing::info!(pane = %pane, "the window it was showing is gone");
            self.detach_pane(pane, None);
        }
    }

    fn schedule_frame(&mut self) {
        let handle = self.handle.clone();
        schedule_frame(&handle, self);
    }

    fn panes_pending(&self) -> bool {
        self.panes.iter().any(|(pane, attached)| {
            self.state
                .should_present_view(*pane, attached.presenter.is_ready())
        })
    }

    fn flush_clients(&mut self) {
        if let Err(err) = self.display.flush_clients() {
            tracing::warn!(?err, "flushing to clients failed");
        }
    }

    fn accept_terminal(&mut self, stream: UnixStream) {
        self.next_pane = PaneId::new(self.next_pane.into_inner().saturating_add(1));
        let pane = self.next_pane;
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
            .spawn(move || read_terminal(stream, sender, pane, write_half))
        {
            Ok(reader) => {
                self.readers.insert(pane, reader);
            }
            Err(error) => tracing::warn!(%error, "could not start a terminal reader"),
        }
    }

    fn on_terminal(&mut self, message: FromTerminal) {
        // Readers may send after a pane detaches.
        let pane = message.pane();
        if !matches!(message, FromTerminal::Hello { .. }) && !self.has_pane(pane) {
            return;
        }
        match message {
            FromTerminal::Hello {
                pane,
                version,
                show,
                capabilities,
                stream,
            } => self.attach_pane(pane, version, show, &capabilities, stream),
            FromTerminal::Input { input, .. } => self.on_input(pane, input),
            FromTerminal::Resized { capabilities, .. } => {
                self.resize_pane(pane, &capabilities);
            }
            FromTerminal::Drawn { .. } => {
                if let Some(attached) = self.panes.get(&pane) {
                    attached.presenter.drawn();
                }
            }
            FromTerminal::Left { .. } => {
                tracing::info!(pane = %pane, "the terminal went away");
                self.detach_pane(pane, None);
            }
        }
        if self.state.take_detach_request(pane) {
            tracing::info!(pane = %pane, "the detach binding was used");
            self.detach_pane(pane, None);
        }
    }

    fn has_pane(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
    }

    fn attach_pane(
        &mut self,
        pane: PaneId,
        version: ProtocolVersion,
        show: display::Show,
        capabilities: &Capabilities,
        mut stream: UnixStream,
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
            drop(self.readers.remove(&pane));
            let _ = display::write_to(
                &mut stream,
                display::encode_client(ToClient::Detached(reason)),
            );
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }

        let Some(reader) = self.readers.remove(&pane) else {
            tracing::warn!("a terminal said hello that was not being read");
            return;
        };
        // Welcome must precede presenter output.
        let _ = display::write_to(&mut stream, display::encode_client(ToClient::Welcome));

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
                            if let Some(attached) = app.panes.get_mut(&pane) {
                                attached.presenter.recycle(frame);
                            }
                        }
                        ChannelEvent::Msg(PresenterEvent::Failed(error)) => {
                            tracing::error!(%error, pane = %pane, "presentation failed");
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

        self.state.attach_view(pane, show, capabilities);
        tracing::info!(pane = %pane, ?capabilities, "pane attached");
        self.panes.insert(
            pane,
            Pane {
                stream,
                presenter,
                reader: Some(reader),
                drawn,
            },
        );
    }

    fn detach_pane(&mut self, pane: PaneId, reason: Option<&str>) {
        let Some(mut attached) = self.panes.remove(&pane) else {
            return;
        };
        if let Some(reason) = reason {
            attached
                .presenter
                .detach(ToClient::Detached(reason.to_owned()));
        }
        // Drain the presenter before closing the socket.
        attached.presenter.finish();
        self.handle.remove(attached.drawn);
        let _ = attached.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = attached.reader
            && reader.join().is_err()
        {
            tracing::error!("the pane's reader panicked");
        }
        self.state.detach_view(pane);
    }

    fn resize_pane(&mut self, pane: PaneId, capabilities: &Capabilities) {
        let Some(attached) = self.panes.get(&pane) else {
            return;
        };
        // Queue the wipe after frames from the old size.
        attached.presenter.clear();
        self.state.resize_view(pane, capabilities);
    }

    fn on_input(&mut self, pane: PaneId, input: Input) {
        match input {
            Input::Key(key) => self.state.key(pane, key),
            Input::Pointer(pointer) => self.state.pointer(pane, pointer),
            Input::Paste(text) => self.state.paste(&text),
            Input::Focus(_) => {}
        }
    }

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
        // Do not leak an inherited X display when the satellite is absent.
        match &self.xwayland {
            Some(server) => child.env("DISPLAY", server.display()),
            None => child.env_remove("DISPLAY"),
        };
        process::spawn_unblocked(&mut child);
        let child = child.spawn()?;
        tracing::info!(program = %program.to_string_lossy(), pid = child.id(), "client started");
        self.children.push(child);
        Ok(())
    }

    fn client_output(&self) -> Stdio {
        match self.log.try_clone() {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                tracing::warn!(%error, "could not send a client's output to the log");
                Stdio::null()
            }
        }
    }

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

    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        if let Some(signal) = &self.signal {
            signal.stop();
        }
    }

    /// Release panes, then stop the process tree.
    fn shutdown(&mut self) {
        while let Some(pane) = self.panes.keys().copied().next() {
            self.detach_pane(pane, None);
        }

        for (signal, grace) in process::ESCALATION {
            let mut tree = process::descendants_of(ProcessId::new(std::process::id()));
            if tree.is_empty() {
                break;
            }
            tracing::info!(signal = ?signal, processes = tree.len(), "stopping the process tree");
            process::signal(&tree, signal);
            if process::wait_until_gone(&mut tree, grace, &mut || self.reap()) {
                break;
            }
            tracing::warn!(
                signal = ?signal,
                processes = ?tree,
                "processes are still running after this signal"
            );
        }

        if let Some(mut xwayland) = self.xwayland.take() {
            xwayland.stop();
        }
        tracing::info!("meowland stopped");
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the reader outlives whoever started it, so it owns its end of the channel rather than borrowing it"
)]
fn read_terminal(
    mut stream: UnixStream,
    sender: Sender<FromTerminal>,
    pane: PaneId,
    write_half: UnixStream,
) {
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
        pane,
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
            ToServer::Input(input) => sender.send(FromTerminal::Input { pane, input }),
            ToServer::Resized(capabilities) => {
                sender.send(FromTerminal::Resized { pane, capabilities })
            }
            ToServer::Drawn => sender.send(FromTerminal::Drawn { pane }),
            ToServer::Bye => sender.send(FromTerminal::Left { pane }),
            ToServer::Hello { .. } => continue,
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = sender.send(FromTerminal::Left { pane });
}

struct Sources {
    clients: ListeningSocketSource,
    terminals: UnixListener,
    commands: UnixListener,
    signals: Signals,
    said: Channel<FromTerminal>,
}

fn install_sources(
    handle: &LoopHandle<'_, App>,
    app: &mut App,
    sources: Sources,
) -> Result<(), Error> {
    watch_clients(handle, sources.clients)?;
    watch_display(handle, &mut app.display)?;
    watch_terminal(handle, sources.said)?;
    watch_server(handle, sources.signals, sources.commands, sources.terminals)?;
    app.schedule_frame();
    Ok(())
}

/// A source that the event loop refused, named so that the failure can be told
/// from the others.
fn watch<T: std::fmt::Debug>(source: &'static str, refused: T) -> Error {
    Error::Watch {
        source,
        cause: format!("{refused:?}").into(),
    }
}

fn watch_clients(handle: &LoopHandle<'_, App>, socket: ListeningSocketSource) -> Result<(), Error> {
    handle
        .insert_source(socket, |stream, (), app| {
            if let Err(err) = app.state.insert_client(stream) {
                tracing::warn!(?err, "could not adopt a client");
            }
        })
        .map_err(|refused| watch("the Wayland socket", refused))?;
    Ok(())
}

fn watch_display(
    handle: &LoopHandle<'_, App>,
    display: &mut Display<Meowland>,
) -> Result<(), Error> {
    let poll_fd = rustix::io::dup(display.backend().poll_fd()).map_err(|err| Error::Watch {
        source: "the display socket",
        cause: err.into(),
    })?;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                let dispatched = app.display.dispatch_clients(&mut app.state);
                if let Err(err) = dispatched {
                    tracing::warn!(?err, "dispatching to clients failed");
                }
                app.settled();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|refused| watch("the display", refused))?;
    Ok(())
}

fn watch_terminal(
    handle: &LoopHandle<'_, App>,
    terminal_events: Channel<FromTerminal>,
) -> Result<(), Error> {
    handle
        .insert_source(terminal_events, |event, (), app: &mut App| {
            match event {
                ChannelEvent::Msg(message) => app.on_terminal(message),
                ChannelEvent::Closed => app.stop(),
            }
            app.settled();
        })
        .map_err(|refused| watch("the terminals", refused))?;
    Ok(())
}

fn watch_server(
    handle: &LoopHandle<'_, App>,
    signals: Signals,
    control_listener: UnixListener,
    display_listener: UnixListener,
) -> Result<(), Error> {
    handle
        .insert_source(signals, |event, (), app: &mut App| match event.signal() {
            Signal::SIGCHLD => app.reap(),
            _ => app.stop(),
        })
        .map_err(|refused| watch("process signals", refused))?;

    handle
        .insert_source(
            Generic::new(control_listener, Interest::READ, Mode::Level),
            |_, listener, app: &mut App| {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // The event loop draws frames, so give up on a
                            // connection that says nothing, rather than wait
                            // for it.
                            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                            let mut request = Vec::new();
                            if let Err(error) = stream.read_to_end(&mut request) {
                                tracing::warn!(%error, "could not read control request");
                                continue;
                            }
                            // A connection that says nothing asks whether a
                            // server is there. The connection itself is the
                            // answer.
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
        .map_err(|refused| watch("the control socket", refused))?;

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
        .map_err(|refused| watch("the terminal socket", refused))?;
    Ok(())
}

/// What the frames of one second cost this thread, logged so that a slow frame
/// rate is attributed and not guessed at. This is the thread that reads input,
/// so time spent here is time that a keystroke waits.
///
/// The presenter keeps its own count of the compressing and the writing.
#[derive(Debug, Default)]
struct FrameStats {
    report: logging::Report,
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

        let Some(seconds) = self.report.due() else {
            return;
        };
        let frames = f64::from(self.frames);
        tracing::debug!(
            fps = frames / seconds,
            tiles = self.tiles / u64::from(self.frames),
            sent = self.sent / u64::from(self.frames),
            compose_ms = self.compose.as_secs_f64() * 1e3 / frames,
            "frames composed"
        );
        *self = Self {
            report: self.report,
            ..Self::default()
        };
    }
}

/// The earliest deadline that holds the frame cap and adds no idle latency.
///
/// `last_frame_started` is when the previous frame began. A frame that takes
/// longer than the interval is late, but the next frame is not pushed out by
/// the work that the late frame already paid for.
fn frame_deadline(now: Instant, last_frame_started: Option<Instant>) -> Instant {
    last_frame_started
        .and_then(|started| started.checked_add(FRAME_INTERVAL))
        .map_or(now, |deadline| deadline.max(now))
}

fn schedule_frame(handle: &LoopHandle<'_, App>, app: &mut App) {
    if app.stopping {
        return;
    }
    // A server with no pane attached keeps running, but it has nothing to draw.
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
        // A frame that took longer than the interval has paid for its own time.
        // The next frame goes as soon as there is something to show, and not a
        // further interval after the work finished.
        let now = Instant::now();
        let started = now
            .checked_sub(FRAME_INTERVAL + Duration::from_millis(10))
            .expect("the monotonic clock has advanced past startup");
        assert_eq!(frame_deadline(now, Some(started)), now);
    }
}
