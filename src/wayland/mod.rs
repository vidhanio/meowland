//! The compositor: the Wayland clients, their protocol state, and the frames
//! they add up to.
//!
//! It runs on a thread of its own and is reached only in messages, so that
//! composing a frame never makes an input event wait: see [`message`] for what
//! crosses between it and the server.
//!
//! Each Wayland client is a connection on the [`ListeningSocketSource`] the
//! server bound. Their requests are dispatched here, and what the server needs
//! to know about them — a window opening, being named, taking the keyboard,
//! closing — is sent as it happens, addressed by the window or pane it is
//! about rather than by client.

pub mod buffer;
pub mod gpu;
pub mod message;
pub mod state;

mod run;

use std::thread::{self, JoinHandle};

use calloop::channel::{Sender, channel};
use smithay::wayland::socket::ListeningSocketSource;

use crate::{
    Error,
    dmabuf::RenderNode,
    wayland::message::{Command, Event},
};

/// The compositor's end of the messages.
///
/// Dropping this stops the compositor.
#[derive(Debug)]
pub struct Wayland {
    commands: Option<Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl Wayland {
    /// Start the compositor on a thread of its own, listening on `socket`.
    ///
    /// Everything that can fail about starting up is done before this returns,
    /// so a compositor that cannot come up is reported here rather than at the
    /// first client.
    pub fn start(
        socket: ListeningSocketSource,
        nodes: Vec<RenderNode>,
        events: Sender<Event>,
    ) -> Result<Self, Error> {
        let (commands, queue) = channel();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("meowland-wayland".into())
            .spawn(move || run::run(socket, &nodes, events, queue, ready))?;
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                commands: Some(commands),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::CompositorPanicked)
            }
        }
    }

    /// Tell the compositor what happened.
    pub fn send(&self, command: Command) {
        if let Some(commands) = &self.commands
            && commands.send(command).is_err()
        {
            tracing::warn!("the compositor is gone");
        }
    }

    /// Stop the compositor, and wait for it to flush its clients and leave.
    pub fn finish(&mut self) {
        self.commands = None;
        if let Some(handle) = self.thread.take()
            && handle.join().is_err()
        {
            tracing::error!("the compositor thread panicked");
        }
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        self.finish();
    }
}
