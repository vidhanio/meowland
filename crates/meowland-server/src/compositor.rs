//! Render-thread ownership and direct requests to its single-owner state.

use std::{io, sync::mpsc, thread};

use calloop::channel;
use meowland_compositor::{Command, Compositor, Event, Runtime};
use meowland_core::scale::UiScale;
use meowland_protocol::REQUEST_TIMEOUT;
use tokio::sync::{mpsc as asynchronous, oneshot};

enum Operation {
    Command(Command),
    Call(Box<dyn FnOnce(&mut Compositor) + Send>),
}

pub struct DisplayThread {
    operations: channel::Sender<Operation>,
    pub events: asynchronous::UnboundedReceiver<Event>,
    pub display: String,
    thread: Option<thread::JoinHandle<()>>,
}

impl DisplayThread {
    pub fn start(scale: UiScale) -> io::Result<Self> {
        let display = format!("wayland-meowland-{}", std::process::id());
        let socket = display.clone();
        let (operations, incoming) = channel::channel::<Operation>();
        let (events_tx, events) = asynchronous::unbounded_channel();
        let (ready_tx, ready) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("meowland-wayland".into())
            .spawn(move || {
                let mut runtime = match Runtime::new(&socket, scale) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if let Err(error) =
                    runtime
                        .handle()
                        .insert_source(incoming, |event, (), compositor| match event {
                            channel::Event::Msg(Operation::Command(command)) => {
                                compositor.handle_command(command);
                            }
                            channel::Event::Msg(Operation::Call(operation)) => {
                                operation(compositor);
                            }
                            channel::Event::Closed => compositor.handle_command(Command::Shutdown),
                        })
                {
                    let _ = ready_tx.send(Err(io::Error::from(error.error)));
                    return;
                }
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while !runtime.compositor().stopped() {
                    if let Err(error) = runtime.dispatch(None) {
                        tracing::error!(%error, "compositor dispatch stopped");
                        break;
                    }
                    for event in runtime.compositor().drain_events() {
                        if events_tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            })?;
        let worker = Self {
            operations,
            events,
            display,
            thread: Some(thread),
        };
        ready
            .recv()
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))??;
        Ok(worker)
    }

    pub fn execute(
        &self,
        operation: impl FnOnce(&mut Compositor) + Send + 'static,
    ) -> io::Result<()> {
        self.operations
            .send(Operation::Call(Box::new(operation)))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    pub fn command(&self, command: Command) -> io::Result<()> {
        self.operations
            .send(Operation::Command(command))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    pub fn call<T: Send + 'static>(
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

    pub fn stop(&mut self) -> io::Result<()> {
        let _ = self.command(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("compositor thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for DisplayThread {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(%error, "cannot stop compositor");
        }
    }
}
