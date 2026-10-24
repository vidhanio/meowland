//! The server actor exclusively owns policy, clients, and request deadlines.

use std::{
    collections::HashMap,
    io,
    time::{Duration, Instant},
};

use meowland_core::{Error, Result, asynchronous::Descriptor, signals};
use meowland_protocol::Paths;
use tokio::{io::AsyncReadExt, sync::mpsc, task::JoinSet};

use super::{
    Server, compositor,
    control::Requests,
    lifecycle::{CompositorStatus, Shutdown},
    notify,
    process::Children,
    transport,
};

const MAX_PANES: usize = 256;
const MAX_CONTROLS: usize = 64;
const MAX_HANDSHAKES: usize = 32;
const INCOMING_QUEUE: usize = 64;
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
enum ConnectionKind {
    Control,
    Pane,
}

pub async fn serve(paths: &Paths, scale: meowland_core::scale::UiScale) -> Result<()> {
    let listeners = transport::listeners(paths)?;
    let interrupted = signals::server_termination_flag()?;
    let wakeup = signals::Wakeup::new(&[
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGCHLD,
    ])?;
    let descriptor = Descriptor::new(rustix::io::dup(&wakeup).map_err(io::Error::from)?)?;
    notify(&[sd_notify::NotifyState::Status("Starting compositor")]);
    let compositor::Handle {
        commands,
        events,
        display,
        thread: compositor_thread,
    } = tokio::task::spawn_blocking(move || compositor::spawn_scaled(scale))
        .await
        .map_err(io::Error::other)??;
    let (incoming, messages) = mpsc::channel(INCOMING_QUEUE);
    let mut server = Server {
        incoming,
        messages,
        commands,
        events,
        display,
        panes: HashMap::new(),
        windows: HashMap::new(),
        children: Children::default(),
        requests: Requests::new(),
        next_pane: 1,
        listeners: Some(listeners),
        controls: JoinSet::new(),
        handshakes: JoinSet::new(),
        connections: JoinSet::new(),
        accept_retry: None,
        shutdown: Shutdown::Running,
        compositor: CompositorStatus::Running,
        interrupted,
    };
    notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Accepting control and pane connections"),
    ]);
    let result = server.run(descriptor).await;
    if result.is_err() {
        server.begin_shutdown();
        server.force_shutdown();
    }
    server.listeners.take();
    server.panes.clear();
    server.controls.shutdown().await;
    server.handshakes.shutdown().await;
    server.connections.shutdown().await;
    let joined = tokio::task::spawn_blocking(move || compositor_thread.join())
        .await
        .map_err(io::Error::other)?;
    server.children.reap();
    result?;
    if server.compositor.failed() || joined.is_err() {
        return Err(Error::io(
            "compositor thread stopped unexpectedly",
            io::Error::other("display unavailable"),
        ));
    }
    Ok(())
}

impl Server {
    async fn run(&mut self, mut wakeup: Descriptor) -> io::Result<()> {
        let mut signals = [0; 256];
        loop {
            if self.interrupted.interrupted() {
                self.begin_shutdown();
            }
            self.expire_deadlines();
            if self.shutdown.stopped() {
                return Ok(());
            }
            let deadline = self.next_deadline();
            let accepting = self.shutdown.running() && self.accept_retry.is_none();
            let control_capacity = self.controls.len() < MAX_CONTROLS;
            let pane_capacity = self.handshakes.len() < MAX_HANDSHAKES
                && self.handshakes.len() + self.connections.len() < MAX_PANES;
            tokio::select! {
                result = wakeup.read(&mut signals) => { result?; self.children.reap(); }
                () = sleep_until(deadline) => {}
                Some(message) = self.messages.recv() => self.incoming(message),
                event = self.events.recv(), if self.compositor.alive() => {
                    if let Some(event) = event { self.compositor_event(event); }
                    else { self.compositor_gone(); }
                }
                Some(result) = self.controls.join_next(), if !self.controls.is_empty() => {
                    match result {
                        Ok(Err(error)) => tracing::debug!(%error, "control connection ended"),
                        Err(error) if error.is_panic() => tracing::error!(%error, "control task panicked"),
                        _ => {}
                    }
                }
                Some(result) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    match result {
                        Ok(Ok(pane)) if self.shutdown.running() => self.attach_pane(pane),
                        Ok(Err(error)) => tracing::debug!(%error, "pane handshake ended"),
                        Err(error) if error.is_panic() => tracing::error!(%error, "pane handshake panicked"),
                        _ => {}
                    }
                }
                Some(result) = self.connections.join_next(), if !self.connections.is_empty() => {
                    match result {
                        Ok(id) => self.pane_gone(id),
                        Err(error) => {
                            if error.is_panic() { tracing::error!(%error, "pane task panicked"); }
                            let id = self.panes.iter().find(|(_, pane)| pane.task_id() == error.id()).map(|(id, _)| *id);
                            if let Some(id) = id { self.pane_gone(id); }
                        }
                    }
                }
                result = transport::accept(self.listeners.as_ref().map(|listeners| &listeners.control)), if accepting && control_capacity => {
                    self.accepted(ConnectionKind::Control, result)?;
                }
                result = transport::accept(self.listeners.as_ref().map(|listeners| &listeners.pane)), if accepting && pane_capacity => {
                    self.accepted(ConnectionKind::Pane, result)?;
                }
            }
        }
    }

    fn accepted(
        &mut self,
        kind: ConnectionKind,
        result: io::Result<tokio::net::UnixStream>,
    ) -> io::Result<()> {
        match result {
            Ok(socket) => match kind {
                ConnectionKind::Control => {
                    self.controls
                        .spawn(transport::control(socket, self.incoming.clone()));
                }
                ConnectionKind::Pane => {
                    self.handshakes.spawn(transport::handshake(socket));
                }
            },
            Err(error) if transient_accept(&error) => {
                tracing::warn!(%error, "accept failed; retrying");
                self.accept_retry = Some(Instant::now() + ACCEPT_RETRY);
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.requests
            .next_deadline()
            .into_iter()
            .chain(self.shutdown.deadline())
            .chain(self.accept_retry)
            .min()
    }

    fn expire_deadlines(&mut self) {
        let now = Instant::now();
        self.requests.expire(now);
        if self.accept_retry.is_some_and(|deadline| deadline <= now) {
            self.accept_retry = None;
        }
        if self
            .shutdown
            .deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.advance_shutdown();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if !self.shutdown.stopped() {
            self.force_shutdown();
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn transient_accept(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .iter()
        .any(|candidate| Some(candidate.raw_os_error()) == error.raw_os_error())
}
