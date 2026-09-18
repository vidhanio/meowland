//! The server: the process that outlives every terminal.
//!
//! It owns the panes attached to it, the control socket, the client processes
//! it started, xwayland-satellite, and the compositor thread it talks to in
//! messages. A pane's frame goes from the compositor to that pane's presenter;
//! everything else goes through here.

pub mod control;
pub mod launch;
pub mod panes;
pub mod presenter;
pub mod process;
pub mod windows;
pub mod xwayland;

use std::{collections::HashMap, fs::File, process::Child, thread::JoinHandle, time::Duration};

use calloop::{
    EventLoop as Calloop, LoopHandle, LoopSignal,
    channel::{Channel, Event as ChannelEvent, Sender, channel},
    signals::{Signal, Signals},
};
use smithay::wayland::socket::ListeningSocketSource;

use crate::{
    Error,
    cli::Settings,
    logging,
    protocol::{
        PaneId,
        control::{CONTROL_SOCKET, Socket},
        pane,
    },
    server::{
        panes::{FromPane, Pane},
        process::ProcessId,
        windows::Windows,
    },
    wayland::{
        Wayland,
        message::{Command, Event},
    },
};

/// How long a client is given to close its windows on its own before it is
/// signalled. Long enough for a client to take down what it has open, and short
/// enough that stopping a server still feels immediate.
const CLOSE_GRACE: Duration = Duration::from_millis(250);

pub fn run(settings: Settings) -> Result<(), Error> {
    let Settings {
        gpu_buffers,
        render_node,
        log,
        log_level,
    } = settings;
    let log = logging::init(log.as_deref(), log_level.as_deref())?;

    let (control_socket, control_listener) = Socket::bind(CONTROL_SOCKET)?;
    let (display_socket, pane_listener) = pane::listen()?;
    let signals = Signals::new(&[
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])?;

    let socket = bind_socket()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    tracing::info!(socket = %socket_name, "meowland server starting");

    let nodes = gpu_buffers.nodes(render_node.as_deref())?;
    let (event_sender, events) = channel();
    let wayland = Wayland::start(socket, nodes, event_sender)?;
    let xwayland = match xwayland::Server::start(&socket_name) {
        Ok(server) => Some(server),
        Err(error) => {
            tracing::warn!(%error, "X11 clients will not work: xwayland-satellite did not start");
            None
        }
    };

    let (terminal_sender, terminals) = channel();
    let mut event_loop: Calloop<Server> = Calloop::try_new()?;
    let handle = event_loop.handle();
    let mut server = Server {
        wayland,
        xwayland,
        socket_name,
        log,
        panes: HashMap::new(),
        windows: Windows::default(),
        terminal_sender,
        readers: HashMap::new(),
        next_pane: PaneId::new(0),
        handle: handle.clone(),
        children: Vec::new(),
        // Removes the sockets when the event loop ends.
        _sockets: (control_socket, display_socket),
        stopping: false,
        signal: None,
    };
    server.signal = Some(event_loop.get_signal());

    control::install(&handle, control_listener)?;
    panes::install(&handle, pane_listener)?;
    panes::watch_reader(&handle, terminals)?;
    watch_wayland(&handle, events)?;
    watch_signals(&handle, signals)?;

    let result = event_loop
        .run(None, &mut server, |_| {})
        .map_err(Error::from);
    server.shutdown();
    result
}

fn bind_socket() -> Result<ListeningSocketSource, Error> {
    ListeningSocketSource::with_name("wayland-meowland")
        .or_else(|_| ListeningSocketSource::new_auto())
        .map_err(Error::WaylandSocket)
}

/// The server: what it owns, and what it is in the middle of.
struct Server {
    /// The compositor, on its own thread. Every wayland message goes through
    /// it.
    wayland: Wayland,
    xwayland: Option<xwayland::Server>,
    socket_name: String,
    log: File,
    panes: HashMap<PaneId, Pane>,
    windows: Windows,
    terminal_sender: Sender<FromPane>,
    /// Readers for terminals that have connected but not said hello yet.
    readers: HashMap<PaneId, JoinHandle<()>>,
    next_pane: PaneId,
    handle: LoopHandle<'static, Self>,
    /// Removes the sockets when the event loop ends.
    _sockets: (Socket, Socket),
    children: Vec<Child>,
    stopping: bool,
    signal: Option<LoopSignal>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("panes", &self.panes.len())
            .field("windows", &self.windows.list().len())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Stop the event loop, once anything in flight has been answered.
    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        if let Some(signal) = &self.signal {
            signal.stop();
        }
    }

    /// Release the panes, stop the clients, and let the compositor go.
    ///
    /// The clients are asked to close their windows first, and given a moment:
    /// one that closes on its own exits with the children of its own, and
    /// nothing has to be signalled. The signals below are for what is left,
    /// and for xwayland-satellite, which has no windows of its own to close.
    fn shutdown(&mut self) {
        self.close_clients();
        while let Some(pane) = self.panes.keys().copied().next() {
            self.detach_pane(pane, None);
        }

        // The clients are stopped before the compositor is joined: a client
        // that is in the middle of a request is the one thing that can hold its
        // thread up, and the signal below is what takes it away.
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

        self.wayland.finish();
        if let Some(mut xwayland) = self.xwayland.take() {
            xwayland.stop();
        }
        tracing::info!("meowland stopped");
    }

    /// Something happened in the compositor: a window, a pane, or a frame.
    fn on_wayland(&mut self, event: Event) {
        match event {
            Event::Named {
                window,
                label,
                title,
            } => self.windows.named(window, label, title),
            Event::Focused { window } => self.windows.focused(window),
            Event::Closed { window } => self.windows.closed(window),
            Event::PaneDone { pane, reason } => {
                tracing::info!(pane = %pane, ?reason, "the pane has nothing left to show");
                self.detach_pane(pane, reason.as_deref());
            }
            Event::Title { pane, title } => self.title(pane, &title),
            Event::Pointer { pane, shape } => self.pointer_shape(pane, shape),
            Event::Frame { pane, frame } => self.present(pane, frame),
        }
    }

    /// Ask every client to close its windows, and wait a moment for the ones
    /// that do.
    fn close_clients(&mut self) {
        self.wayland.send(Command::Close);
        let mut clients: Vec<ProcessId> = self
            .children
            .iter()
            .map(|child| ProcessId::new(child.id()))
            .collect();
        if clients.is_empty() {
            return;
        }
        tracing::info!(clients = clients.len(), "waiting for clients to close");
        if !process::wait_until_gone(&mut clients, CLOSE_GRACE, &mut || self.reap()) {
            tracing::info!(clients = ?clients, "some clients did not close on their own");
        }
    }

    /// Take back the children that have exited.
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
            Err(error) => {
                tracing::warn!(?error, "could not wait for a client");
                false
            }
        });
    }
}

/// What the compositor says it did.
fn watch_wayland(
    handle: &LoopHandle<'static, Server>,
    events: Channel<Event>,
) -> Result<(), Error> {
    handle
        .insert_source(events, |event, (), server: &mut Server| match event {
            ChannelEvent::Msg(event) => server.on_wayland(event),
            ChannelEvent::Closed => server.stop(),
        })
        .map_err(|refused| watch("the compositor", refused))?;
    Ok(())
}

fn watch_signals(handle: &LoopHandle<'static, Server>, signals: Signals) -> Result<(), Error> {
    handle
        .insert_source(signals, |event, (), server: &mut Server| {
            match event.signal() {
                Signal::SIGCHLD => server.reap(),
                _ => server.stop(),
            }
        })
        .map_err(|refused| watch("process signals", refused))?;
    Ok(())
}

/// A source that the event loop refused, named so that the failure can be told
/// from the others.
pub fn watch<T: std::fmt::Debug>(source: &'static str, refused: T) -> Error {
    Error::Watch {
        source,
        cause: format!("{refused:?}").into(),
    }
}
