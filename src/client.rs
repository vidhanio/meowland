//! Showing a server on the terminal this process runs in.
//!
//! This module is the other end of [`crate::display`], and the only part of
//! meowland that touches a terminal. It takes the terminal over, reports what
//! the terminal can do, writes out what the server sends, and forwards what the
//! user does. The server outlives this process: closing the terminal, or
//! `Alt+Q`, leaves the server running with its windows and its clients.
//!
//! Frames go out on their own thread: the wait for the terminal is
//! milliseconds, and the thread reading the user must not queue behind it.

use std::{
    io::Write as _,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use calloop::{
    EventLoop, LoopSignal,
    channel::{Event as ChannelEvent, Sender, channel},
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};

use crate::{
    display::{self, Input, Key, KeyKind, Pointer, Show, ToClient, ToServer},
    keys,
    tty::{self, Capabilities, Terminal},
};

/// How long the server has to answer a terminal that attaches. It answers at
/// once, so a second means it is wedged.
const GREETING_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the loop checks for a hangup.
///
/// A closed terminal fails every read, which the input thread cannot report:
/// the terminal library spins on the error. So the loop asks, and the reader
/// spins until this process ends.
const HANGUP_INTERVAL: Duration = Duration::from_secs(1);

/// Show a window of the server in this terminal until something ends the
/// attachment.
///
/// `show` selects the window: the one the server has the keyboard on, the
/// newest one, or one by ID. A terminal that asks for the newest window follows
/// new windows as they appear. Terminals are independent, so any number of them
/// can be attached at once, showing the same window or one each. Only one
/// terminal's showing ends here, never a window or another terminal's.
pub fn attach(show: Show) -> Result<(), Error> {
    // Before the terminal is touched: a server that is not there must not clear
    // the screen.
    let mut stream = display::connect()?;
    let mut terminal = Terminal::new()?;
    let capabilities = terminal.activate()?.clone();
    tracing::info!(?capabilities, "terminal taken over");

    let departure = match greet(&mut stream, capabilities, show)? {
        // The terminal goes with it, and is given back on the way out.
        Greeting::Welcome => display_it(stream, terminal)?,
        Greeting::Refused(reason) => {
            drop(terminal);
            Departure::Detached(reason)
        }
        Greeting::Gone => {
            drop(terminal);
            Departure::ServerGone
        }
    };

    // Reported after the terminal is back. Before that, the message goes to the
    // compositor's screen, not the user's.
    match departure {
        Departure::Detached(reason) => {
            tracing::info!(%reason, "let go of the server");
            eprintln!("{reason}");
            Ok(())
        }
        // Not an error. The server was stopped, whether by `meowland server stop`
        // or by a signal. The terminal is given back, and the shell that typed the
        // command carries on.
        Departure::ServerGone => {
            tracing::info!("the server stopped");
            Ok(())
        }
        Departure::TerminalGone => {
            tracing::info!("the terminal went away");
            Ok(())
        }
        Departure::Signal => Ok(()),
    }
}

/// Why the terminal side did not finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server could not be reached.
    #[error(transparent)]
    Control(#[from] crate::control::Error),
    /// The terminal could not be taken over.
    #[error(transparent)]
    Terminal(#[from] tty::Error),
    /// The hello could not be sent, or its answer could not be read.
    #[error("could not reach the server")]
    Server(#[source] std::io::Error),
    /// The server sent pixels before it said hello.
    #[error("the server sent pixels before it said hello")]
    PixelsFirst,
    /// The event loop could not be made, or refused a source.
    #[error("could not make an event loop")]
    EventLoop(#[source] calloop::Error),
    /// A source the terminal side needs could not be watched.
    #[error("could not watch {source}")]
    Watch {
        source: &'static str,
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The signals of this process could not be watched.
    #[error("could not listen for signals")]
    Signals(#[source] calloop::Error),
    /// The thread that writes frames, or the one that reads the user, did not
    /// start.
    #[error("could not start {thread}")]
    Thread {
        thread: &'static str,
        #[source]
        cause: std::io::Error,
    },
    /// The socket could not be cloned for a thread of its own.
    #[error("could not take a copy of the server's socket")]
    Clone(#[source] std::io::Error),
    /// Showing the server ended in failure.
    #[error("showing the server failed")]
    Showing(#[source] calloop::Error),
}

/// The server's answer to a terminal that attaches.
enum Greeting {
    Welcome,
    Refused(String),
    /// Nothing arrived: the server stopped before it answered.
    Gone,
}

/// Send hello, and read whether the server accepted the terminal.
fn greet(
    stream: &mut UnixStream,
    capabilities: Capabilities,
    show: Show,
) -> Result<Greeting, Error> {
    display::write_to(
        stream,
        display::encode(&ToServer::Hello {
            version: display::VERSION,
            show,
            capabilities,
        }),
    )
    .map_err(Error::Server)?;

    stream
        .set_read_timeout(Some(GREETING_TIMEOUT))
        .map_err(Error::Server)?;
    let reply = display::read_from(stream);
    stream.set_read_timeout(None).map_err(Error::Server)?;

    let reply = reply.map_err(Error::Server)?;
    match reply.and_then(|(tag, payload)| display::decode_client(tag, payload)) {
        Some(ToClient::Welcome) => Ok(Greeting::Welcome),
        // A refusal is an answer, not a failure. The caller decides what to
        // print.
        Some(ToClient::Detached(reason)) => Ok(Greeting::Refused(reason)),
        // The socket ended before the server sent anything. The server was
        // stopped, or a command that exits at once took it with it.
        None => Ok(Greeting::Gone),
        Some(ToClient::Bytes(_) | ToClient::Frame(_)) => Err(Error::PixelsFirst),
    }
}

/// Why this terminal stopped showing the server.
#[derive(Debug)]
enum Departure {
    /// The server let the terminal go, and gave the reason.
    Detached(String),
    ServerGone,
    TerminalGone,
    Signal,
}

struct Showing {
    stream: UnixStream,
    terminal: Terminal,
    departure: Option<Departure>,
    signal: Option<LoopSignal>,
}

impl Showing {
    /// Leave for `reason`. The first reason given is the one that counts.
    fn leave(&mut self, reason: Departure) {
        if self.departure.is_none() {
            self.departure = Some(reason);
        }
    }

    fn stop(&self) {
        if let Some(signal) = &self.signal {
            signal.stop();
        }
    }

    /// Send one message to the server, and leave if it cannot be sent.
    fn send(&mut self, message: &ToServer) {
        if let Err(error) = display::write_to(&mut self.stream, display::encode(message)) {
            tracing::warn!(%error, "could not reach the server");
            self.leave(Departure::ServerGone);
            self.stop();
        }
    }
}

/// Write frames out and forward input until one end goes away. The terminal is
/// given back at the end.
fn display_it(stream: UnixStream, terminal: Terminal) -> Result<Departure, Error> {
    let mut event_loop: EventLoop<Showing> = EventLoop::try_new().map_err(Error::EventLoop)?;
    let signal = event_loop.get_signal();
    let handle = event_loop.handle();

    let (frames_sender, frames) = channel();
    let stream_clone = stream.try_clone().map_err(Error::Clone)?;
    let mut writer = Worker::start("meowland-frames", move |stop| {
        write_frames(stream_clone, &frames_sender, stop);
    })
    .map_err(|cause| Error::Thread {
        thread: "the frame writer",
        cause,
    })?;

    handle
        .insert_source(frames, |event, (), showing: &mut Showing| match event {
            ChannelEvent::Msg(FrameEvent::Drawn) => {
                // The terminal has the frame: the server may compose the next.
                showing.send(&ToServer::Drawn);
            }
            ChannelEvent::Msg(FrameEvent::Detached(reason)) => {
                showing.leave(Departure::Detached(reason));
                showing.stop();
            }
            ChannelEvent::Msg(FrameEvent::Ended) | ChannelEvent::Closed => {
                showing.leave(Departure::ServerGone);
                showing.stop();
            }
        })
        .map_err(|refused| watch("the server", refused))?;

    let (input_sender, input) = channel();
    let mut input_thread = Worker::start("meowland-input", move |stop| {
        read_terminal(&input_sender, stop);
    })
    .map_err(|cause| Error::Thread {
        thread: "the terminal reader",
        cause,
    })?;
    handle
        .insert_source(input, |event, (), showing: &mut Showing| match event {
            ChannelEvent::Msg(event) => on_event(showing, event),
            ChannelEvent::Closed => {}
        })
        .map_err(|refused| watch("the terminal", refused))?;

    handle
        .insert_source(
            Timer::from_duration(HANGUP_INTERVAL),
            |_, (), showing: &mut Showing| {
                if tty::hung_up() {
                    showing.leave(Departure::TerminalGone);
                    showing.stop();
                }
                TimeoutAction::ToDuration(HANGUP_INTERVAL)
            },
        )
        .map_err(|refused| watch("the hangup timer", refused))?;

    handle
        .insert_source(
            Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP])
                .map_err(Error::Signals)?,
            |_, (), showing: &mut Showing| {
                showing.leave(Departure::Signal);
                showing.stop();
            },
        )
        .map_err(|refused| watch("the signals", refused))?;

    let mut showing = Showing {
        stream,
        terminal,
        departure: None,
        signal: Some(signal),
    };
    let result = event_loop.run(None, &mut showing, |_| {});
    // `Bye` reports that the terminal left on purpose, so a closed terminal is
    // not the same thing to the server as one that fell over.
    let _ = display::write_to(&mut showing.stream, display::encode(&ToServer::Bye));
    let _ = showing.stream.shutdown(std::net::Shutdown::Both);
    writer.stop();
    input_thread.stop();
    result.map_err(Error::Showing)?;
    Ok(showing.departure.unwrap_or(Departure::ServerGone))
}

/// A source that the event loop refused, named so that the failure can be told
/// from the others.
fn watch<T: std::fmt::Debug>(source: &'static str, refused: T) -> Error {
    Error::Watch {
        source,
        cause: format!("{refused:?}").into(),
    }
}

/// One event from the terminal, in the terms the server counts in.
fn on_event(showing: &mut Showing, event: crossterm::event::Event) {
    use crossterm::event::Event;

    // A resize is not input. The server needs the capabilities the terminal has
    // now, and `refresh` probes them.
    if let Event::Resize(_, _) = event {
        let capabilities = showing.terminal.refresh().clone();
        showing.send(&ToServer::Resized(capabilities));
        return;
    }
    if let Some(input) = input_for(event) {
        showing.send(&ToServer::Input(input));
    }
}

fn input_for(event: crossterm::event::Event) -> Option<Input> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};

    match event {
        Event::Key(key) => {
            let Some(stroke) = keys::for_key(key.code) else {
                if let KeyCode::Char(character) = key.code {
                    tracing::debug!(
                        ?character,
                        "character has no key code in the advertised keymap"
                    );
                }
                return None;
            };
            Some(Input::Key(Key {
                code: stroke.code,
                shift: stroke.shift,
                modifiers: key.modifiers.bits(),
                kind: match key.kind {
                    KeyEventKind::Press => KeyKind::Press,
                    KeyEventKind::Repeat => KeyKind::Repeat,
                    KeyEventKind::Release => KeyKind::Release,
                },
                // A modifier is state for everything typed while it is held,
                // not a keystroke of its own.
                modifier: matches!(key.code, KeyCode::Modifier(_)),
            }))
        }
        Event::Mouse(mouse) => Some(Input::Pointer(match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => Pointer::Motion {
                column: mouse.column,
                row: mouse.row,
            },
            MouseEventKind::Down(button) | MouseEventKind::Up(button) => Pointer::Button {
                column: mouse.column,
                row: mouse.row,
                button: match button {
                    MouseButton::Left => keys::button::LEFT,
                    MouseButton::Right => keys::button::RIGHT,
                    MouseButton::Middle => keys::button::MIDDLE,
                },
                pressed: matches!(mouse.kind, MouseEventKind::Down(_)),
            },
            MouseEventKind::ScrollUp => Pointer::ScrollUp,
            MouseEventKind::ScrollDown => Pointer::ScrollDown,
            MouseEventKind::ScrollLeft => Pointer::ScrollLeft,
            MouseEventKind::ScrollRight => Pointer::ScrollRight,
        })),
        // Pasted text is not typed and has no keys of its own to forward.
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::FocusGained => Some(Input::Focus(true)),
        Event::FocusLost => Some(Input::Focus(false)),
        Event::Resize(_, _) => None,
    }
}

/// What the frame-writing thread sends to the loop.
#[derive(Debug)]
enum FrameEvent {
    Drawn,
    /// The server let this terminal go, and gave the reason.
    Detached(String),
    Ended,
}

/// A thread of this process's own, for one of the two blocking jobs: sending
/// frames to the terminal, or reading the user.
///
/// A write goes at the terminal's pace; a read waits for the user to type.
/// Neither can be the loop's own body, so each is left by asking.
struct Worker {
    name: &'static str,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(
        name: &'static str,
        body: impl FnOnce(&AtomicBool) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || body(&thread_stop))?;
        Ok(Self {
            name,
            stop,
            handle: Some(handle),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return;
        };
        // A thread inside a hung-up terminal never comes back: it spins in the
        // terminal library's read, or blocks on a pty with no reader, so a join
        // waits forever. It holds nothing this process needs.
        if tty::hung_up() {
            return;
        }
        if handle.join().is_err() {
            tracing::error!(thread = self.name, "the thread panicked");
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_frames(mut stream: UnixStream, events: &Sender<FrameEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let message = match display::read_from(&mut stream) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "the server stopped sending frames");
                break;
            }
        };
        let (bytes, frame) = match display::decode_client(message.0, message.1) {
            Some(ToClient::Bytes(bytes)) => (bytes, false),
            Some(ToClient::Frame(bytes)) => (bytes, true),
            Some(ToClient::Detached(reason)) => {
                let _ = events.send(FrameEvent::Detached(reason));
                return;
            }
            // A message this build does not understand is not a reason to stop.
            Some(ToClient::Welcome) | None => continue,
        };
        let phase = std::time::Instant::now();
        // Taken per message, not held for the session: a write the terminal is
        // slow to take must not block the escapes that give the terminal back.
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
            tracing::debug!(%error, "could not write to the terminal");
            let _ = events.send(FrameEvent::Ended);
            return;
        }
        tracing::trace!(
            bytes = bytes.len(),
            ms = phase.elapsed().as_secs_f64() * 1e3,
            "written to the terminal"
        );
        // Only a frame is acknowledged. The server composes the next frame
        // against the screen this one landed on. An escape is not worth a wait.
        if frame && events.send(FrameEvent::Drawn).is_err() {
            return;
        }
    }
    let _ = events.send(FrameEvent::Ended);
}

/// crossterm parses the escape sequences, including the kitty keyboard protocol
/// extensions meowland asks for.
fn read_terminal(sender: &Sender<crossterm::event::Event>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                tracing::debug!(%error, "terminal input ended");
                return;
            }
        }
        match crossterm::event::read() {
            Ok(event) => {
                if sender.send(event).is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "terminal input ended");
                return;
            }
        }
    }
}
