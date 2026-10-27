//! Render-thread ownership and direct requests to its single-owner state.

use std::{io, sync::mpsc, thread};

use calloop::{
    channel,
    ping::{Ping, PingSource, make_ping},
};
use meowland_compositor::{Command, Compositor, Event, Runtime};
use meowland_core::scale::UiScale;
use meowland_protocol::REQUEST_TIMEOUT;
use tokio::sync::{mpsc as asynchronous, oneshot};

const QUEUE_CAPACITY: usize = 1024;

enum Operation {
    Command(Command),
    Call(Box<dyn FnOnce(&mut Compositor) + Send>),
}

pub(crate) struct DisplayThread {
    operations: channel::SyncSender<Operation>,
    shutdown: Ping,
    pub events: asynchronous::Receiver<Event>,
    pub display: String,
    thread: Option<thread::JoinHandle<()>>,
}

impl DisplayThread {
    pub(crate) fn start(scale: UiScale) -> io::Result<Self> {
        let display = format!("wayland-meowland-{}", std::process::id());
        let socket = display.clone();
        let (operations, incoming) = channel::sync_channel(QUEUE_CAPACITY);
        let (shutdown, stopping) = make_ping()?;
        let (events_tx, events) = asynchronous::channel(QUEUE_CAPACITY);
        let (ready_tx, ready) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("meowland-wayland".into())
            .spawn(move || {
                if let Err(error) = run(&socket, scale, incoming, stopping, &events_tx, &ready_tx) {
                    tracing::error!(%error, "compositor thread stopped");
                    let _ = ready_tx.send(Err(error));
                }
            })?;
        let worker = Self {
            operations,
            shutdown,
            events,
            display,
            thread: Some(thread),
        };
        ready
            .recv()
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))??;
        Ok(worker)
    }

    fn queue(&self, operation: Operation) -> io::Result<()> {
        self.operations.try_send(operation).map_err(|error| {
            match error {
                mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                mpsc::TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe,
            }
            .into()
        })
    }

    pub(crate) fn execute(
        &self,
        operation: impl FnOnce(&mut Compositor) + Send + 'static,
    ) -> io::Result<()> {
        self.queue(Operation::Call(Box::new(operation)))
    }

    pub(crate) fn command(&self, command: Command) -> io::Result<()> {
        if matches!(command, Command::Shutdown) {
            self.shutdown.ping();
            Ok(())
        } else {
            self.queue(Operation::Command(command))
        }
    }

    pub(crate) fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Compositor) -> T + Send + 'static,
    ) -> impl Future<Output = io::Result<T>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        let queued = self.execute(move |compositor| {
            if !sender.is_closed() {
                let _ = sender.send(operation(compositor));
            }
        });
        async move {
            queued?;
            tokio::time::timeout(REQUEST_TIMEOUT, receiver)
                .await?
                .map_err(io::Error::other)
        }
    }

    pub(crate) async fn stop(&mut self) -> io::Result<()> {
        self.shutdown.ping();
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(io::Error::other)?
                .map_err(|_| io::Error::other("compositor thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for DisplayThread {
    fn drop(&mut self) {
        self.shutdown.ping();
    }
}

fn run(
    socket: &str,
    scale: UiScale,
    incoming: channel::Channel<Operation>,
    stopping: PingSource,
    events: &asynchronous::Sender<Event>,
    ready: &mpsc::Sender<io::Result<()>>,
) -> io::Result<()> {
    let mut runtime = Runtime::new(socket, scale)?;
    runtime
        .handle()
        .insert_source(incoming, |event, (), compositor| match event {
            channel::Event::Msg(Operation::Command(command)) => compositor.handle_command(command),
            channel::Event::Msg(Operation::Call(operation)) => operation(compositor),
            channel::Event::Closed => compositor.handle_command(Command::Shutdown),
        })
        .map_err(|error| io::Error::from(error.error))?;
    runtime
        .handle()
        .insert_source(stopping, |(), (), compositor| {
            compositor.handle_command(Command::Shutdown);
        })
        .map_err(|error| io::Error::from(error.error))?;
    if ready.send(Ok(())).is_err() {
        return Ok(());
    }
    while !runtime.compositor().stopped() {
        runtime.dispatch(None)?;
        for event in runtime.compositor().drain_events() {
            match events.try_send(event) {
                Ok(()) => {}
                Err(asynchronous::error::TrySendError::Closed(_)) => return Ok(()),
                Err(asynchronous::error::TrySendError::Full(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "compositor event queue exhausted",
                    ));
                }
            }
        }
    }
    Ok(())
}
