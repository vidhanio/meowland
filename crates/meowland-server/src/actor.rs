//! Event-loop setup and fair progress for the uniquely owned server state.

use std::{collections::HashMap, io, os::unix::net::UnixStream, time::Instant};

use ::futures::{
    StreamExt,
    channel::mpsc,
    future::{AbortHandle, Abortable},
};
use calloop::{
    Dispatcher, EventLoop, Interest, Mode, PostAction, futures,
    generic::Generic,
    signals::{Signal, Signals},
};
use meowland_compositor::{CompositorHandle, Runtime, Settings};
use meowland_protocol::Paths;

use super::{
    Completion, ConnectionId, Error, Listener, MAX_CONNECTIONS, Result, Server,
    lifecycle::Shutdown,
    notify, sockets,
    transport::{self, ConnectionKind},
};

pub(crate) fn serve(paths: &Paths, settings: &Settings) -> Result<()> {
    let listeners = sockets::listeners(paths)
        .map_err(|error| error.with_context("setting up control and pane sockets"))?;
    let event_loop = EventLoop::<Server>::try_new()
        .map_err(|error| Error::io("creating the event loop", io::Error::from(error)))?;
    let handle = event_loop.handle();
    let signals = Signals::new(&[
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])
    .map_err(|error| Error::io("installing signal handling", io::Error::from(error)))?;
    notify(&[sd_notify::NotifyState::Status("Starting compositor")]);
    let display = format!("wayland-meowland-{}", std::process::id());
    let (wayland, compositor) = Runtime::with_settings(
        &display,
        settings,
        &CompositorHandle::new(handle.clone(), |server| &mut server.compositor),
    )
    .map_err(|error| Error::io("starting the Wayland compositor", error))?;
    let (executor, scheduler) = futures::executor()
        .map_err(|error| Error::io("creating the async executor", io::Error::from(error)))?;
    let executor = Dispatcher::new(
        executor,
        |completion, (), server: &mut Server| match completion {
            Completion::Incoming(message, receiver) => {
                if let Some(message) = message {
                    server.incoming(message);
                    server.receive(receiver);
                }
            }
            Completion::Connection(id, result) => server.connection_finished(id, result),
        },
    );
    let executor_token = handle
        .register_dispatcher(executor.clone())
        .map_err(|error| Error::io("registering the async executor", io::Error::from(error)))?;
    handle
        .insert_source(signals, |event, (), server| match event.signal() {
            Signal::SIGCHLD => server.reap_children(),
            Signal::SIGINT | Signal::SIGTERM => server.begin_shutdown(),
            _ => {}
        })
        .map_err(|error| Error::io("registering signal handling", io::Error::from(error.error)))?;
    let mut registered = Vec::new();
    for (socket, kind) in [
        (listeners.control, ConnectionKind::Control),
        (listeners.pane, ConnectionKind::Pane),
    ] {
        let action = match kind {
            ConnectionKind::Control => "registering the control socket",
            ConnectionKind::Pane => "registering the pane socket",
        };
        let token = handle
            .insert_source(
                Generic::new(socket, Interest::READ, Mode::Level),
                move |_, listener, server| {
                    server.accepted(kind, listener.accept());
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|error| Error::io(action, io::Error::from(error.error)))?;
        registered.push(Listener {
            token,
            kind,
            enabled: true,
        });
    }
    let (incoming, messages) = mpsc::channel(MAX_CONNECTIONS);
    let server = Server {
        compositor,
        wayland,
        display,
        handle,
        scheduler,
        executor: Some((executor_token, executor)),
        incoming,
        listeners: registered,
        connections: HashMap::new(),
        panes: HashMap::new(),
        next_connection: 1,
        shortcuts: settings.shortcuts,
        children: Vec::new(),
        shutdown: Shutdown::Running,
        error: None,
    };
    server.receive(messages);
    notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Accepting control and pane connections"),
    ]);
    run(event_loop, server)
}

fn run(mut event_loop: EventLoop<'static, Server>, mut server: Server) -> Result<()> {
    let mut events = Vec::new();
    while !server.shutdown.stopped() {
        server.advance_shutdown(Instant::now());
        if server.shutdown.stopped() {
            break;
        }
        server
            .update_listeners()
            .map_err(|error| Error::io("updating listener state", error))?;
        let timeout = Runtime::next_wakeup(&server.compositor)
            .into_iter()
            .chain(
                server
                    .shutdown
                    .deadline()
                    .map(|deadline| deadline.saturating_duration_since(Instant::now())),
            )
            .min();
        event_loop
            .dispatch(timeout, &mut server)
            .map_err(|error| Error::io("running the event loop", io::Error::from(error)))?;
        server
            .wayland
            .dispatch(&mut server.compositor)
            .map_err(|error| Error::io("dispatching Wayland clients", error))?;
        events.extend(server.compositor.drain_events());
        #[expect(
            clippy::iter_with_drain,
            reason = "reuse the event buffer across dispatch turns"
        )]
        for event in events.drain(..) {
            server.event(event);
        }
        if server.compositor.stopped() {
            server.begin_shutdown();
        }
    }
    server.reap_children();
    if let Some(error) = server.error.take() {
        return Err(error);
    }
    Ok(())
}

impl Server {
    fn receive(&self, mut receiver: mpsc::Receiver<super::Incoming>) {
        self.scheduler
            .schedule(async move { Completion::Incoming(receiver.next().await, receiver) })
            .expect("executor lives for the server loop");
    }

    fn update_listeners(&mut self) -> io::Result<()> {
        for listener in &mut self.listeners {
            let capacity =
                MAX_CONNECTIONS - usize::from(matches!(listener.kind, ConnectionKind::Pane));
            let enabled = self.shutdown.running() && self.connections.len() < capacity;
            if enabled != listener.enabled {
                if enabled {
                    self.handle.enable(&listener.token)?;
                } else {
                    self.handle.disable(&listener.token)?;
                }
                listener.enabled = enabled;
            }
        }
        Ok(())
    }

    fn accepted(&mut self, kind: ConnectionKind, result: io::Result<UnixStream>) {
        let capacity = MAX_CONNECTIONS - usize::from(matches!(kind, ConnectionKind::Pane));
        if !self.shutdown.running() || self.connections.len() >= capacity {
            return;
        }
        let socket = match result {
            Ok(socket) => socket,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return;
            }
            Err(error) => {
                self.error = Some(Error::io("accepting a connection", error));
                self.begin_shutdown();
                return;
            }
        };
        let id = ConnectionId(self.next_connection);
        self.next_connection += 1;
        let incoming = self.incoming.clone();
        let handle = self.handle.clone();
        let (abort, registration) = AbortHandle::new_pair();
        self.connections.insert(id, abort);
        self.scheduler
            .schedule(async move {
                let result = Abortable::new(
                    transport::connection(socket, kind, id, incoming, handle),
                    registration,
                )
                .await
                .unwrap_or_else(|_| Err(io::ErrorKind::ConnectionAborted.into()));
                Completion::Connection(id, result)
            })
            .expect("executor lives for the server loop");
    }
}
