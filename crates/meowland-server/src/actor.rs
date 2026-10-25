//! A single I/O owner for connections, child processes and shutdown.

use std::{collections::HashMap, io, time::Instant};

use meowland_compositor::PaneId;
use meowland_core::{Result, asynchronous::Descriptor, scale::UiScale, signals};
use meowland_protocol::Paths;
use tokio::{io::AsyncReadExt, net::UnixStream, sync::mpsc, task::JoinSet};

use super::{
    MAX_CONNECTIONS, Server,
    compositor::DisplayThread,
    lifecycle::Shutdown,
    notify,
    process::Children,
    sockets,
    transport::{self, ConnectionKind},
};

pub async fn serve(paths: &Paths, scale: UiScale) -> Result<()> {
    let listeners = sockets::listeners(paths)?;
    let interrupted = signals::server_termination_flag()?;
    let wakeup = signals::Wakeup::new(&[
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGCHLD,
    ])?;
    let descriptor = Descriptor::new(rustix::io::dup(&wakeup).map_err(io::Error::from)?)?;
    notify(&[sd_notify::NotifyState::Status("Starting compositor")]);
    let compositor = DisplayThread::start(scale)?;
    let (incoming, messages) = mpsc::channel(MAX_CONNECTIONS);
    let mut server = Server {
        compositor,
        incoming,
        messages,
        listeners: Some(listeners),
        connections: HashMap::new(),
        tasks: JoinSet::new(),
        panes: HashMap::new(),
        next_connection: 1,
        children: Children::default(),
        shutdown: Shutdown::Running,
    };
    notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Accepting control and pane connections"),
    ]);
    let result = server.run(descriptor, &interrupted).await;
    server.listeners.take();
    server.tasks.shutdown().await;
    server.panes.clear();
    let stopped = server.compositor.stop();
    result?;
    stopped?;
    Ok(())
}

impl Server {
    async fn run(
        &mut self,
        mut wakeup: Descriptor,
        interrupted: &signals::TerminationFlag,
    ) -> io::Result<()> {
        let mut signals = [0; 256];
        loop {
            if interrupted.interrupted() {
                self.begin_shutdown();
            }
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
                result = wakeup.read(&mut signals) => { result?; self.children.reap(); }
                () = sleep_until(deadline) => {}
                Some(message) = self.messages.recv() => self.incoming(message).await,
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
