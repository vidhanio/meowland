//! A single I/O owner for connections, child processes and shutdown.

use std::collections::HashMap;
use std::io;
use std::time::Instant;

use meowland_compositor::PaneId;
use meowland_core::Result;
use meowland_core::scale::UiScale;
use meowland_core::signals::{Termination, TerminationSignal};
use meowland_protocol::Paths;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::compositor::DisplayThread;
use super::lifecycle::Shutdown;
use super::transport::{self, ConnectionKind};
use super::{MAX_CONNECTIONS, Server, notify, sockets};

pub(crate) async fn serve(paths: &Paths, scale: UiScale) -> Result<()> {
    let listeners = sockets::listeners(paths)?;
    let termination = Termination::new()?;
    notify(&[sd_notify::NotifyState::Status("Starting compositor")]);
    let compositor = tokio::task::spawn_blocking(move || DisplayThread::start(scale))
        .await
        .map_err(io::Error::other)??;
    let (incoming, messages) = mpsc::channel(MAX_CONNECTIONS);
    let mut server = Server {
        compositor,
        incoming,
        messages,
        listeners: Some(listeners),
        connections: HashMap::new(),
        tasks: JoinSet::new(),
        controls: JoinSet::new(),
        panes: HashMap::new(),
        next_connection: 1,
        children: JoinSet::new(),
        shutdown: Shutdown::Running,
    };
    notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Accepting control and pane connections"),
    ]);
    let result = server.run(termination).await;
    server.listeners.take();
    server.controls.shutdown().await;
    server.tasks.shutdown().await;
    server.children.shutdown().await;
    server.panes.clear();
    let stopped = server.compositor.stop().await;
    result?;
    stopped?;
    Ok(())
}

impl Server {
    async fn run(&mut self, mut termination: Termination) -> io::Result<()> {
        loop {
            self.advance_shutdown(Instant::now());
            if self.shutdown.stopped() {
                return Ok(());
            }
            let deadline = self.shutdown.deadline();
            let control_capacity =
                self.shutdown.running() && self.connections.len() < MAX_CONNECTIONS;
            let pane_capacity =
                self.shutdown.running() && self.connections.len() < MAX_CONNECTIONS - 1;
            tokio::select! {
                signal = termination.recv() => {
                    if signal != TerminationSignal::Hangup {
                        self.begin_shutdown();
                    }
                }
                Some(result) = self.children.join_next(), if !self.children.is_empty() => {
                    result.map_err(io::Error::other)?;
                }
                () = sleep_until(deadline) => {}
                Some(message) = self.messages.recv() => self.incoming(message),
                Some(result) = self.controls.join_next(), if !self.controls.is_empty() => {
                    match result {
                        Ok(completion) => self.control_finished(completion),
                        Err(error) if error.is_cancelled() => {},
                        Err(error) => return Err(io::Error::other(error)),
                    }
                }
                event = self.compositor.events.recv() => {
                    self.event(event.ok_or_else(|| io::Error::other("compositor stopped unexpectedly"))?);
                }
                Some(result) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    match result {
                        Ok((id, result)) => self.connection_finished(id, result),
                        Err(error) if error.is_cancelled() => {},
                        Err(error) => return Err(io::Error::other(error)),
                    }
                }
                result = sockets::accept(self.listeners.as_ref().map(|listeners| &listeners.control)), if control_capacity => {
                    self.accepted(ConnectionKind::Control, result)?;
                }
                result = sockets::accept(self.listeners.as_ref().map(|listeners| &listeners.pane)), if pane_capacity => {
                    self.accepted(ConnectionKind::Pane, result)?;
                }
            }
        }
    }

    fn accepted(&mut self, kind: ConnectionKind, result: io::Result<UnixStream>) -> io::Result<()> {
        let socket = match result {
            Ok(socket) => socket,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let id = PaneId(self.next_connection);
        self.next_connection += 1;
        let incoming = self.incoming.clone();
        let task = self
            .tasks
            .spawn(async move { (id, transport::connection(socket, kind, id, incoming).await) });
        self.connections.insert(id, task);
        Ok(())
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    } else {
        std::future::pending::<()>().await;
    }
}
