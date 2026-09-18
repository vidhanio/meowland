//! The panes attached to the server: one terminal each.
//!
//! A pane is a connection from a terminal. The server reads what that terminal
//! sent on a thread of its own, keeps a presenter to write frames back, and
//! tells the compositor to show a window in it. Everything that decides what
//! the pane shows belongs to the compositor; everything that decides whether
//! the pane is there at all belongs here.

use std::{
    io::BufReader,
    os::unix::net::UnixStream,
    thread::{self, JoinHandle},
    time::Duration,
};

use calloop::{
    LoopHandle, RegistrationToken,
    channel::{Event as ChannelEvent, Sender, channel},
};
use smithay::wayland::socket::ListeningSocketSource;

use crate::{
    Error, kitty,
    protocol::{
        PaneId, ProtocolVersion,
        pane::{self, Capabilities, Hello, Input, Show, ToServer},
    },
    server::{Server, presenter, presenter::Presenter, watch},
    wayland::message::Command,
};

/// How long a pane's hello has to arrive before its reader gives up, so that a
/// connection that says nothing does not hold the event loop.
const HELLO_TIMEOUT: Duration = Duration::from_secs(1);

/// One attached terminal, and the presenter that writes frames to it.
#[derive(Debug)]
pub(super) struct Pane {
    stream: UnixStream,
    presenter: Presenter,
    reader: Option<JoinHandle<()>>,
    /// The source that returns this pane's frames to the compositor.
    free: RegistrationToken,
}

/// What a pane's reader thread tells the server.
#[derive(Debug)]
pub(super) enum FromPane {
    Hello {
        pane: PaneId,
        version: ProtocolVersion,
        show: Show,
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
    /// The terminal has written the last frame.
    Drawn {
        pane: PaneId,
    },
    Left {
        pane: PaneId,
    },
}

impl FromPane {
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

pub(super) fn install(
    handle: &LoopHandle<'static, Server>,
    listener: ListeningSocketSource,
) -> Result<(), Error> {
    handle
        .insert_source(listener, |stream, (), server: &mut Server| {
            server.accept_terminal(stream);
        })
        .map_err(|refused| watch("the pane socket", refused))?;
    Ok(())
}

pub(super) fn watch_reader(
    handle: &LoopHandle<'static, Server>,
    terminal_events: calloop::channel::Channel<FromPane>,
) -> Result<(), Error> {
    handle
        .insert_source(
            terminal_events,
            |event, (), server: &mut Server| match event {
                ChannelEvent::Msg(message) => server.on_pane_message(message),
                ChannelEvent::Closed => server.stop(),
            },
        )
        .map_err(|refused| watch("the terminals", refused))?;
    Ok(())
}

impl Server {
    /// A terminal has connected, but has not said who it is yet.
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
            .name("meowland-pane".to_owned())
            .spawn(move || read_pane(stream, sender, pane, write_half))
        {
            Ok(reader) => {
                self.readers.insert(pane, reader);
            }
            Err(error) => tracing::warn!(%error, "could not start a terminal reader"),
        }
    }

    fn on_pane_message(&mut self, message: FromPane) {
        // Readers may send after a pane detaches.
        let pane = message.pane();
        if !matches!(message, FromPane::Hello { .. }) && !self.panes.contains_key(&pane) {
            return;
        }
        match message {
            FromPane::Hello {
                pane,
                version,
                show,
                capabilities,
                stream,
            } => self.attach_pane(pane, version, show, &capabilities, stream),
            FromPane::Input { input, .. } => {
                self.wayland.send(Command::Input { pane, input });
            }
            FromPane::Resized { capabilities, .. } => self.resize_pane(pane, &capabilities),
            FromPane::Drawn { .. } => {
                if let Some(attached) = self.panes.get(&pane) {
                    attached.presenter.drawn();
                }
            }
            FromPane::Left { .. } => {
                tracing::info!(pane = %pane, "the terminal went away");
                self.detach_pane(pane, None);
            }
        }
    }

    /// A terminal has said hello: welcome it, and show it a window.
    fn attach_pane(
        &mut self,
        pane: PaneId,
        version: ProtocolVersion,
        show: Show,
        capabilities: &Capabilities,
        mut stream: UnixStream,
    ) {
        let refusal = if version != pane::VERSION {
            Some(format!(
                "this server speaks version {} of the protocol, and this terminal speaks {version}",
                pane::VERSION
            ))
        } else if let Show::Window(id) = show
            && !self.windows.has(id)
        {
            Some("no window has that ID".to_owned())
        } else {
            None
        };
        if let Some(reason) = refusal {
            tracing::info!(%reason, "turned a terminal away");
            drop(self.readers.remove(&pane));
            let _ = pane::write_detached(&mut stream, &reason);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }

        let Some(reader) = self.readers.remove(&pane) else {
            tracing::warn!("a terminal said hello that was not being read");
            return;
        };
        // Welcome must precede presenter output.
        let _ = pane::write_welcome(&mut stream);

        let draw_on = match stream.try_clone() {
            Ok(half) => half,
            Err(error) => {
                tracing::warn!(%error, "could not take a terminal's socket");
                return;
            }
        };
        let (free_sender, free) = channel();
        let presenter = match Presenter::new(free_sender) {
            Ok(presenter) => presenter,
            Err(error) => {
                tracing::warn!(%error, "could not start a presenter for a pane");
                return;
            }
        };
        presenter.attach(draw_on, capabilities);
        let free = match self
            .handle
            .clone()
            .insert_source(free, move |event, (), server| {
                match event {
                    // The terminal has taken the frame: the compositor draws
                    // the next one into it.
                    ChannelEvent::Msg(presenter::Event::Free { frame }) => {
                        server.wayland.send(Command::Frame { pane, frame });
                    }
                    ChannelEvent::Msg(presenter::Event::Failed(error)) => {
                        tracing::error!(%error, pane = %pane, "presentation failed");
                        // The pane cannot be drawn to any more, and its frame
                        // is gone with the thread: let
                        // go of it rather than leave its
                        // clients waiting for a frame that will not come.
                        server.detach_pane(pane, None);
                    }
                    ChannelEvent::Closed => {}
                }
            }) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "could not watch a pane's presenter");
                return;
            }
        };

        self.panes.insert(
            pane,
            Pane {
                stream,
                presenter,
                reader: Some(reader),
                free,
            },
        );
        tracing::info!(pane = %pane, ?capabilities, "pane attached");
        self.wayland.send(Command::Attach {
            pane,
            show,
            capabilities: capabilities.clone(),
        });
    }

    /// Let go of a pane: drain its presenter, close its socket, and tell the
    /// compositor to stop drawing for it.
    pub(super) fn detach_pane(&mut self, pane: PaneId, reason: Option<&str>) {
        let Some(mut attached) = self.panes.remove(&pane) else {
            return;
        };
        if let Some(reason) = reason {
            attached.presenter.detach(reason);
        }
        // Drain the presenter before closing the socket.
        attached.presenter.finish();
        self.handle.remove(attached.free);
        let _ = attached.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = attached.reader
            && reader.join().is_err()
        {
            tracing::error!("the pane's reader panicked");
        }
        self.wayland.send(Command::Detach { pane });
    }

    fn resize_pane(&self, pane: PaneId, capabilities: &Capabilities) {
        let Some(attached) = self.panes.get(&pane) else {
            return;
        };
        // Queue the wipe after the frames from the old size, and before the
        // frames of the new one.
        attached.presenter.configure(capabilities);
        attached.presenter.clear();
        self.wayland.send(Command::Resized {
            pane,
            capabilities: capabilities.clone(),
        });
    }

    /// What the terminal in this pane should call itself.
    pub(super) fn title(&self, pane: PaneId, title: &str) {
        if let Some(attached) = self.panes.get(&pane) {
            attached.presenter.raw(kitty::title(title));
        }
    }

    /// The pointer shape the terminal in this pane should draw.
    pub(super) fn pointer_shape(&self, pane: PaneId, shape: Option<&str>) {
        if let Some(attached) = self.panes.get(&pane) {
            attached.presenter.raw(kitty::pointer_shape_bytes(shape));
        }
    }

    /// Hand a composed frame to its pane's presenter.
    pub(super) fn present(&self, pane: PaneId, frame: crate::render::Frame) {
        if let Some(attached) = self.panes.get(&pane) {
            attached.presenter.present(frame);
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the reader outlives whoever started it, so it owns its end of the channel rather than borrowing it"
)]
fn read_pane(stream: UnixStream, sender: Sender<FromPane>, pane: PaneId, write_half: UnixStream) {
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    // One read fills this, so a message's fields do not cost a read each.
    let mut stream = BufReader::new(stream);
    let hello = pane::read::<_, ToServer>(&mut stream);
    let _ = stream.get_ref().set_read_timeout(None);
    let Ok(ToServer::Hello(Hello {
        version,
        show,
        capabilities,
    })) = hello
    else {
        return;
    };
    let hello = FromPane::Hello {
        pane,
        version,
        show,
        capabilities,
        stream: write_half,
    };
    if sender.send(hello).is_err() {
        return;
    }

    while let Ok(message) = pane::read::<_, ToServer>(&mut stream) {
        let sent = match message {
            ToServer::Input(input) => sender.send(FromPane::Input { pane, input }),
            ToServer::Resized(capabilities) => {
                sender.send(FromPane::Resized { pane, capabilities })
            }
            ToServer::Drawn => sender.send(FromPane::Drawn { pane }),
            ToServer::Bye => {
                // The terminal is leaving: the server hears it once, and
                // nothing more is read from it.
                let _ = sender.send(FromPane::Left { pane });
                return;
            }
            // The first message is read above, before the pane has a name.
            ToServer::Hello(_) => continue,
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = sender.send(FromPane::Left { pane });
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::UnixStream,
        sync::mpsc::TryRecvError,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::protocol::pane::{Hello, ToServer};

    fn capabilities() -> Capabilities {
        Capabilities {
            cell: (10, 20),
            cells: (120, 40),
            pixels: (1200, 800),
            terminal: None,
            graphics: true,
            keyboard: true,
            pixel_mouse: false,
            shared_memory: false,
            patches: true,
        }
    }

    /// What the server heard next, or a panic if nothing comes.
    fn heard(messages: &calloop::channel::Channel<FromPane>) -> FromPane {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match messages.try_recv() {
                Ok(message) => return message,
                Err(TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "the reader is quiet");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TryRecvError::Disconnected) => panic!("the reader is gone"),
            }
        }
    }

    #[test]
    fn what_a_pane_sends_is_what_the_server_hears() {
        // The whole of a pane's way in, over a socket: the hello that names it,
        // what the user did, and the end when the terminal goes away.
        let (mut pane, server) = UnixStream::pair().expect("a socket pair");
        let write_half = server.try_clone().expect("another handle on the socket");
        let (sender, messages) = calloop::channel::channel();
        let reader = thread::spawn(move || read_pane(server, sender, PaneId::new(7), write_half));

        pane::write(
            &mut pane,
            &ToServer::Hello(Hello {
                version: pane::VERSION,
                show: Show::Newest,
                capabilities: capabilities(),
            }),
        )
        .expect("the hello goes out");
        let FromPane::Hello {
            pane: at,
            version,
            show,
            ..
        } = heard(&messages)
        else {
            panic!("a pane says hello first");
        };
        assert_eq!(at, PaneId::new(7));
        assert_eq!(version, pane::VERSION);
        assert_eq!(show, Show::Newest);

        pane::write(&mut pane, &ToServer::Input(Input::Focus(true))).expect("an input goes out");
        assert!(matches!(heard(&messages), FromPane::Input { .. }));

        pane::write(&mut pane, &ToServer::Drawn).expect("an acknowledgement goes out");
        assert!(matches!(heard(&messages), FromPane::Drawn { .. }));

        // A terminal that closes is a pane that has left.
        drop(pane);
        assert!(matches!(heard(&messages), FromPane::Left { .. }));
        reader.join().expect("the reader ends");
    }
}
