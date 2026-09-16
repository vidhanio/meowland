//! Showing a server on the terminal this process is running in.
//!
//! This is the other end of [`crate::display`], and the only part of meowland
//! that touches a terminal: it takes the terminal over, says what the terminal
//! can do, writes out what the server sends it, and forwards what the user
//! does. The server outlives this process - closing the terminal, or `Alt+Q`,
//! leaves it running with its windows and its clients - which is what makes the
//! two ends separate programs at all.
//!
//! Frames are written by a thread of their own, and it is the thread that waits
//! for the terminal to take them: that wait is milliseconds, and the thread
//! reading the user's keystrokes must not queue behind it.

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

use anyhow::Context as _;
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

/// How long the server has to answer a terminal that has just attached. It
/// answers at once: a second is a server that is wedged, not a slow one.
const GREETING_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the terminal is checked for having gone away.
///
/// A closed terminal hangs its descriptors up and fails every read on them, and
/// the thread reading input cannot report that: the terminal library's event
/// source spins on the error instead of returning from the read. So the loop
/// asks, and the reader is left spinning until this process ends.
const HANGUP_INTERVAL: Duration = Duration::from_secs(1);

/// Show a window of the server in this terminal until something ends the
/// attachment.
///
/// What is shown is `show`: the window the server has the keyboard on, the
/// newest one - following as others appear - or one by ID. Terminals are
/// independent, so any number of them can be attached at once, showing the same
/// window or one each; what ends here is one terminal's showing, never a window
/// or another terminal's.
pub fn attach(show: Show) -> anyhow::Result<()> {
    // Before the terminal is touched, because a server that is not there is
    // not worth wiping a screen for.
    let mut stream = display::connect()?;
    let mut terminal = Terminal::new()?;
    let capabilities = terminal.activate()?.clone();
    tracing::info!(?capabilities, "terminal taken over");

    let departure = match greet(&mut stream, capabilities, show)? {
        // The terminal goes with it, and is handed back on the way out.
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

    // Said after the terminal is back, or the terminal it is said to is the
    // compositor's screen rather than the user's.
    match departure {
        Departure::Detached(reason) => {
            tracing::info!(%reason, "let go of the server");
            eprintln!("{reason}");
            Ok(())
        }
        // Not an error: a server stops when the command it was started for is
        // gone, and `meowland quit` stops one on purpose. The terminal is
        // handed back and the shell that typed the command carries on.
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

/// What the server said about the terminal that just attached.
enum Greeting {
    Welcome,
    Refused(String),
    /// Nothing: the server stopped before it answered.
    Gone,
}

/// Say hello, and read whether the server took the terminal on.
fn greet(
    stream: &mut UnixStream,
    capabilities: Capabilities,
    show: Show,
) -> anyhow::Result<Greeting> {
    display::write_to(
        stream,
        display::encode(&ToServer::Hello {
            version: display::VERSION,
            show,
            capabilities,
        }),
    )
    .context("could not say hello to the server")?;

    stream
        .set_read_timeout(Some(GREETING_TIMEOUT))
        .context("could not wait for the server")?;
    let reply = display::read_from(stream);
    stream
        .set_read_timeout(None)
        .context("could not wait for frames")?;

    let reply = reply.context("could not read the server's answer")?;
    match reply.and_then(|(tag, payload)| display::decode_client(tag, payload)) {
        Some(ToClient::Welcome) => Ok(Greeting::Welcome),
        // A refusal is an answer rather than a failure: what to print, and
        // whether to complain, is the caller's to decide.
        Some(ToClient::Detached(reason)) => Ok(Greeting::Refused(reason)),
        // The socket ended without the server saying anything. A server stops
        // when the command it was started for is gone, and a command that exits
        // at once does it before a terminal has finished probing the terminal
        // it was typed in.
        None => Ok(Greeting::Gone),
        Some(ToClient::Bytes(_) | ToClient::Frame(_)) => {
            anyhow::bail!("the server sent pixels before it said hello")
        }
    }
}

/// Why this terminal stopped showing the server.
#[derive(Debug)]
enum Departure {
    /// The server let the terminal go, and said why.
    Detached(String),
    /// The server itself is gone.
    ServerGone,
    /// The terminal was closed.
    TerminalGone,
    /// A signal asked for the server to be left.
    Signal,
}

/// The state the event loop carries while a server is being shown.
struct Showing {
    stream: UnixStream,
    terminal: Terminal,
    departure: Option<Departure>,
    signal: Option<LoopSignal>,
}

impl Showing {
    /// Leave for `reason`; the first reason given is the one that counts.
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

    /// Send one message to the server, leaving if it cannot be sent.
    fn send(&mut self, message: &ToServer) {
        if let Err(error) = display::write_to(&mut self.stream, display::encode(message)) {
            tracing::warn!(%error, "could not reach the server");
            self.leave(Departure::ServerGone);
            self.stop();
        }
    }
}

/// Write out what the server sends and forward what the user does, until one
/// of them goes away. The terminal is handed back on the way out.
fn display_it(stream: UnixStream, terminal: Terminal) -> anyhow::Result<Departure> {
    let mut event_loop: EventLoop<Showing> =
        EventLoop::try_new().context("could not make an event loop")?;
    let signal = event_loop.get_signal();
    let handle = event_loop.handle();

    let (frames_sender, frames) = channel();
    let stream_clone = stream.try_clone()?;
    let mut writer = Worker::start("meowland-frames", move |stop| {
        write_frames(stream_clone, &frames_sender, stop);
    })
    .context("could not start the frame writer")?;

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
        .map_err(|error| anyhow::anyhow!("could not watch the server: {error:?}"))?;

    let (input_sender, input) = channel();
    let mut input_thread = Worker::start("meowland-input", move |stop| {
        read_terminal(&input_sender, stop);
    })
    .context("could not read the terminal")?;
    handle
        .insert_source(input, |event, (), showing: &mut Showing| match event {
            ChannelEvent::Msg(event) => on_event(showing, event),
            ChannelEvent::Closed => {}
        })
        .map_err(|error| anyhow::anyhow!("could not watch the terminal: {error:?}"))?;

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
        .map_err(|error| anyhow::anyhow!("could not watch the terminal: {error:?}"))?;

    handle
        .insert_source(
            Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP])
                .context("could not listen for signals")?,
            |_, (), showing: &mut Showing| {
                showing.leave(Departure::Signal);
                showing.stop();
            },
        )
        .context("could not watch for signals")?;

    let mut showing = Showing {
        stream,
        terminal,
        departure: None,
        signal: Some(signal),
    };
    let result = event_loop.run(None, &mut showing, |_| {});
    // The server is told this was on purpose, so that a terminal that is
    // closed is not the same thing to it as a terminal that fell over.
    let _ = display::write_to(&mut showing.stream, display::encode(&ToServer::Bye));
    let _ = showing.stream.shutdown(std::net::Shutdown::Both);
    writer.stop();
    input_thread.stop();
    result.context("showing the server failed")?;
    Ok(showing.departure.unwrap_or(Departure::ServerGone))
}

/// One event from the terminal, in the terms the server counts in.
fn on_event(showing: &mut Showing, event: crossterm::event::Event) {
    use crossterm::event::Event;

    // A resize is not something the server is told about as input: what it
    // needs is what the terminal can do now, which is a probe away.
    if let Event::Resize(_, _) = event {
        let capabilities = showing.terminal.refresh().clone();
        showing.send(&ToServer::Resized(capabilities));
        return;
    }
    if let Some(input) = input_for(event) {
        showing.send(&ToServer::Input(input));
    }
}

/// What the terminal should be told about one terminal event.
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
                // rather than a keystroke of its own.
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
        // Pasted text is not typed, and has no keys of its own to forward.
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::FocusGained => Some(Input::Focus(true)),
        Event::FocusLost => Some(Input::Focus(false)),
        Event::Resize(_, _) => None,
    }
}

/// What the thread that writes frames tells the loop.
#[derive(Debug)]
enum FrameEvent {
    /// The terminal has written the last frame.
    Drawn,
    /// The server let this terminal go, and said why.
    Detached(String),
    /// The server is gone.
    Ended,
}

/// A thread of this process's own, for one of the two things that cannot happen
/// on the loop: writing frames to the terminal, and reading the user at it.
///
/// Both are inside a blocking call - a write that the terminal sets the pace
/// of, a read that has nothing to return until the user types - so neither can
/// be the loop's own body, and both are left by asking rather than by waiting
/// for them.
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
        // A thread inside a terminal that has hung up never comes back: it is
        // spinning in the terminal library's read of it, or blocked writing to
        // a pty nobody is reading, and waiting for it would be waiting forever.
        // It holds nothing the rest of this process needs, and the process is
        // leaving anyway.
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

/// Write what the server sends to the terminal, until either end goes away.
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
            // Nothing this build understands is nothing to stop for.
            Some(ToClient::Welcome) | None => continue,
        };
        let phase = std::time::Instant::now();
        // Taken per message rather than held for the session: a write that the
        // terminal is not keeping up with must not keep the escapes that hand
        // it back from being written.
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
        // Only a frame is acknowledged: the server composes the next one
        // against the screen this landed on, while an escape is not worth
        // waiting for.
        if frame && events.send(FrameEvent::Drawn).is_err() {
            return;
        }
    }
    let _ = events.send(FrameEvent::Ended);
}

/// Read terminal events forever. crossterm parses the escape sequences,
/// including the kitty keyboard protocol extensions meowland asks for.
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
