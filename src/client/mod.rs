//! One terminal pane: the terminal end of the pane protocol.
//!
//! A pane takes over the terminal it was started in, says hello to the server
//! over the pane socket, and from then on writes the frames the server sends it
//! and sends back what the user did. A pane is a process of its own, so the
//! server outlives it.

pub mod terminal;

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

use self::terminal::Terminal;
use crate::{
    Error, keys,
    protocol::pane::{
        self, Capabilities, Hello, Input, Key, KeyKind, Pointer, Show, ToClient, ToServer,
    },
};

/// How long the pane waits for the server to answer its hello.
const GREETING_TIMEOUT: Duration = Duration::from_secs(1);

/// Crossterm can spin after a terminal closes instead of reporting EOF.
const HANGUP_INTERVAL: Duration = Duration::from_secs(1);

/// Show a window in this terminal, until the terminal or the server is gone.
pub fn attach(show: Show) -> Result<(), Error> {
    // Connect before taking over the terminal.
    let mut stream = pane::connect()?;
    let mut terminal = terminal::Terminal::new()?;
    let capabilities = terminal.activate()?.clone();
    tracing::info!(?capabilities, "terminal taken over");

    let departure = match greet(&mut stream, capabilities, show)? {
        Greeting::Welcome => run(stream, terminal)?,
        Greeting::Refused(reason) => {
            drop(terminal);
            Departure::Detached(reason)
        }
        Greeting::Gone => {
            drop(terminal);
            Departure::ServerGone
        }
    };

    // Report departure after restoring the terminal.
    match departure {
        Departure::Detached(reason) => {
            tracing::info!(%reason, "let go of the server");
            eprintln!("{reason}");
            Ok(())
        }
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

enum Greeting {
    Welcome,
    Refused(String),
    Gone,
}

/// Say hello, and hear whether this pane may show a window here.
fn greet(
    stream: &mut UnixStream,
    capabilities: Capabilities,
    show: Show,
) -> Result<Greeting, Error> {
    pane::write(
        stream,
        &ToServer::Hello(Hello {
            version: pane::VERSION,
            show,
            capabilities,
        }),
    )?;

    stream.set_read_timeout(Some(GREETING_TIMEOUT))?;
    let reply = pane::read::<_, ToClient>(stream);
    stream.set_read_timeout(None)?;

    match reply {
        Ok(ToClient::Welcome) => Ok(Greeting::Welcome),
        Ok(ToClient::Detached(reason)) => Ok(Greeting::Refused(reason)),
        // A server that says nothing, or stops mid-sentence, is a server that
        // is not there: either way there is no window for this pane.
        Err(error) => {
            tracing::debug!(%error, "the server did not answer the hello");
            Ok(Greeting::Gone)
        }
        Ok(ToClient::Bytes(_) | ToClient::Frame(_)) => Err(Error::PixelsFirst),
    }
}

/// Why the pane stopped showing its window.
#[derive(Debug)]
enum Departure {
    Detached(String),
    ServerGone,
    TerminalGone,
    Signal,
}

/// Write frames until either end is gone, and report why it ended.
fn run(stream: UnixStream, terminal: Terminal) -> Result<Departure, Error> {
    let mut event_loop: EventLoop<Showing> = EventLoop::try_new()?;
    let signal = event_loop.get_signal();
    let handle = event_loop.handle();

    let (frames_sender, frames) = channel();
    let stream_clone = stream.try_clone()?;
    let mut writer = Worker::start("meowland-frames", move |stop| {
        write_frames(stream_clone, &frames_sender, stop);
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
                if terminal::hung_up() {
                    showing.leave(Departure::TerminalGone);
                    showing.stop();
                }
                TimeoutAction::ToDuration(HANGUP_INTERVAL)
            },
        )
        .map_err(|refused| watch("the hangup timer", refused))?;

    handle
        .insert_source(
            Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP])?,
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
    let _ = pane::write(&mut showing.stream, &ToServer::Bye);
    let _ = showing.stream.shutdown(std::net::Shutdown::Both);
    writer.stop();
    input_thread.stop();
    result?;
    Ok(showing.departure.unwrap_or(Departure::ServerGone))
}

fn watch<T: std::fmt::Debug>(source: &'static str, refused: T) -> Error {
    Error::Watch {
        source,
        cause: format!("{refused:?}").into(),
    }
}

/// One pane that is showing something: its socket, its terminal, and why it is
/// on its way out.
struct Showing {
    stream: UnixStream,
    terminal: Terminal,
    departure: Option<Departure>,
    signal: Option<LoopSignal>,
}

impl Showing {
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

    fn send(&mut self, message: &ToServer) {
        if let Err(error) = pane::write(&mut self.stream, message) {
            tracing::warn!(%error, "could not reach the server");
            self.leave(Departure::ServerGone);
            self.stop();
        }
    }
}

fn on_event(showing: &mut Showing, event: crossterm::event::Event) {
    use crossterm::event::Event;

    if let Event::Resize(_, _) = event {
        let capabilities = showing.terminal.refresh().clone();
        showing.send(&ToServer::Resized(capabilities));
        return;
    }
    if let Some(input) = input_for(event) {
        showing.send(&ToServer::Input(input));
    }
}

/// What the terminal reported, as the server is told about it.
///
/// The terminal reduces a key to a code and a shift here, because it has the
/// key codes and the keymap.
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
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::FocusGained => Some(Input::Focus(true)),
        Event::FocusLost => Some(Input::Focus(false)),
        Event::Resize(_, _) => None,
    }
}

/// What the thread that writes frames has to say.
#[derive(Debug)]
enum FrameEvent {
    Drawn,
    Detached(String),
    Ended,
}

/// A thread that is stopped by a flag, because it may be inside a read that
/// never returns.
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
        if terminal::hung_up() {
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

/// Write the escapes the server sends to the terminal, and say when a frame is
/// through.
fn write_frames(mut stream: UnixStream, events: &Sender<FrameEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let message = match pane::read::<_, ToClient>(&mut stream) {
            Ok(message) => message,
            // A message this build cannot read, and a server that has gone,
            // end the same way: there is nothing more for this terminal.
            Err(error) => {
                tracing::debug!(%error, "the server stopped sending frames");
                break;
            }
        };
        let (bytes, frame) = match message {
            ToClient::Bytes(bytes) => (bytes, false),
            ToClient::Frame(bytes) => (bytes, true),
            ToClient::Detached(reason) => {
                let _ = events.send(FrameEvent::Detached(reason));
                return;
            }
            ToClient::Welcome => continue,
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
        if frame && events.send(FrameEvent::Drawn).is_err() {
            return;
        }
    }
    let _ = events.send(FrameEvent::Ended);
}

/// Read the terminal's input, and hand it to the event loop.
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
