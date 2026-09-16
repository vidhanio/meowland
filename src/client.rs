//! Terminal-side pane connection.

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
    Error,
    display::{self, Input, Key, KeyKind, Pointer, Show, ToClient, ToServer},
    keys,
    tty::{self, Capabilities, Terminal},
};

const GREETING_TIMEOUT: Duration = Duration::from_secs(1);

// Crossterm can spin after a terminal closes instead of reporting EOF.
const HANGUP_INTERVAL: Duration = Duration::from_secs(1);

pub fn attach(show: Show) -> Result<(), Error> {
    // Connect before taking over the terminal.
    let mut stream = display::connect()?;
    let mut terminal = Terminal::new()?;
    let capabilities = terminal.activate()?.clone();
    tracing::info!(?capabilities, "terminal taken over");

    let departure = match greet(&mut stream, capabilities, show)? {
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
    )?;

    stream.set_read_timeout(Some(GREETING_TIMEOUT))?;
    let reply = display::read_from(stream);
    stream.set_read_timeout(None)?;

    let reply = reply?;
    match reply.and_then(|(tag, payload)| display::decode_client(tag, payload)) {
        Some(ToClient::Welcome) => Ok(Greeting::Welcome),
        Some(ToClient::Detached(reason)) => Ok(Greeting::Refused(reason)),
        None => Ok(Greeting::Gone),
        Some(ToClient::Bytes(_) | ToClient::Frame(_)) => Err(Error::PixelsFirst),
    }
}

#[derive(Debug)]
enum Departure {
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
        if let Err(error) = display::write_to(&mut self.stream, display::encode(message)) {
            tracing::warn!(%error, "could not reach the server");
            self.leave(Departure::ServerGone);
            self.stop();
        }
    }
}

fn display_it(stream: UnixStream, terminal: Terminal) -> Result<Departure, Error> {
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
    let _ = display::write_to(&mut showing.stream, display::encode(&ToServer::Bye));
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

#[derive(Debug)]
enum FrameEvent {
    Drawn,
    Detached(String),
    Ended,
}

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
        if frame && events.send(FrameEvent::Drawn).is_err() {
            return;
        }
    }
    let _ = events.send(FrameEvent::Ended);
}

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
