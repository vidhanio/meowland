//! Per-pane frame encoding and output thread.
//!
//! The compositor draws a pane's whole screen; this thread encodes it as one
//! kitty graphics image and writes it to the pane's socket. It runs on a thread
//! of its own, so a slow terminal holds up only its own pane.

use std::{
    os::unix::net::UnixStream,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{Receiver, Sender as MessageSender, channel},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::channel::Sender as EventSender;

use crate::{
    Error,
    kitty::{self, Encoder},
    protocol::pane::{self, Capabilities},
    render::Frame,
};

#[derive(Debug)]
enum Message {
    Attach(Option<UnixStream>),
    /// Whether the terminal reads frames out of shared memory.
    SharedMemory(bool),
    /// Let go of the pane, and say why.
    Detached(String),
    Frame(Frame),
    Drawn,
    Raw(Vec<u8>),
}

/// What the presenter has to say to the server.
#[derive(Debug)]
pub enum Event {
    /// A frame the terminal has taken, for the compositor to draw the next one
    /// into.
    Free {
        frame: Frame,
    },
    Failed(Error),
}

#[derive(Debug)]
pub struct Presenter {
    messages: Option<MessageSender<Message>>,
    handle: Option<JoinHandle<()>>,
}

impl Presenter {
    pub fn new(events: EventSender<Event>) -> Result<Self, Error> {
        let (messages, queue) = channel();
        let handle = thread::Builder::new()
            .name("meowland-presenter".into())
            .spawn(move || worker(queue, &events))?;
        Ok(Self {
            messages: Some(messages),
            handle: Some(handle),
        })
    }

    /// Give the presenter the terminal's socket, and what the terminal can do.
    pub fn attach(&self, terminal: UnixStream, capabilities: &Capabilities) {
        self.configure(capabilities);
        self.send(Message::Attach(Some(terminal)));
    }

    /// The terminal's shared-memory support changed.
    pub fn configure(&self, capabilities: &Capabilities) {
        self.send(Message::SharedMemory(capabilities.shared_memory));
    }

    pub fn detach(&self, reason: &str) {
        self.send(Message::Detached(reason.to_owned()));
        self.send(Message::Attach(None));
    }

    pub fn clear(&self) {
        self.raw(kitty::clear());
    }

    /// Put a frame on the terminal.
    pub fn present(&self, frame: Frame) {
        self.send(Message::Frame(frame));
    }

    /// The terminal has written the frame: the next one may come.
    pub fn drawn(&self) {
        self.send(Message::Drawn);
    }

    pub fn raw(&self, bytes: Vec<u8>) {
        self.send(Message::Raw(bytes));
    }

    fn send(&self, message: Message) {
        if let Some(messages) = &self.messages {
            let _ = messages.send(message);
        }
    }

    /// Drain queued messages before stopping the worker.
    pub fn finish(&mut self) {
        self.messages = None;
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!("the presenter thread panicked");
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.finish();
    }
}

fn worker(queue: Receiver<Message>, events: &EventSender<Event>) {
    let result = catch_unwind(AssertUnwindSafe(|| run(queue, events)));
    let failure = match result {
        Ok(Ok(())) => return,
        Ok(Err(err)) => Error::Io(err),
        Err(_) => Error::PresenterPanicked,
    };
    let _ = events.send(Event::Failed(failure));
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the worker outlives whoever started it, so it owns its end of the channel rather than borrowing it"
)]
fn run(queue: Receiver<Message>, events: &EventSender<Event>) -> std::io::Result<()> {
    let mut encoder = Encoder::new();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    let mut terminal: Option<UnixStream> = None;
    let mut in_flight: Option<Frame> = None;
    while let Ok(message) = queue.recv() {
        let frame = match message {
            Message::Attach(attached) => {
                terminal = attached;
                if terminal.is_none() {
                    free(&mut in_flight, events)?;
                }
                continue;
            }
            Message::SharedMemory(enabled) => {
                encoder.shared_memory = enabled;
                continue;
            }
            Message::Detached(reason) => {
                write_detached(&mut terminal, &reason)?;
                continue;
            }
            Message::Drawn => {
                free(&mut in_flight, events)?;
                continue;
            }
            Message::Raw(bytes) => {
                write(&mut terminal, &bytes)?;
                continue;
            }
            Message::Frame(frame) => frame,
        };

        // The whole screen goes as one image, under the id the previous frame
        // was sent as: the terminal replaces it as the new one arrives.
        let phase = Instant::now();
        out.clear();
        Encoder::begin_frame(&mut out);
        encoder.transmit(&mut out, frame.pixels(), (frame.width, frame.height));
        Encoder::end_frame(&mut out);
        let spent_encoding = phase.elapsed();

        let phase = Instant::now();
        let written = match write_frame(&mut terminal, &out) {
            Ok(()) => phase.elapsed(),
            Err(error) => {
                // A disconnected pane does not stop the worker.
                tracing::debug!(%error, "the attached terminal is gone");
                terminal = None;
                Duration::ZERO
            }
        };
        if terminal.is_none() {
            if events.send(Event::Free { frame }).is_err() {
                return Ok(());
            }
            continue;
        }

        stats.record(out.len(), spent_encoding, written);
        in_flight = Some(frame);
    }
    Ok(())
}

/// Hand a frame the terminal has taken back to the compositor.
fn free(in_flight: &mut Option<Frame>, events: &EventSender<Event>) -> std::io::Result<()> {
    let Some(frame) = in_flight.take() else {
        return Ok(());
    };
    events
        .send(Event::Free { frame })
        .map_err(|_| std::io::Error::other("the event loop is gone"))
}

fn write(terminal: &mut Option<UnixStream>, bytes: &[u8]) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    pane::write_bytes(terminal, bytes)
}

fn write_frame(terminal: &mut Option<UnixStream>, bytes: &[u8]) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    pane::write_frame(terminal, bytes)
}

fn write_detached(terminal: &mut Option<UnixStream>, reason: &str) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    pane::write_detached(terminal, reason)
}

/// What the frames of one second cost this thread, logged so that a slow
/// terminal is attributed and not guessed at. The compositor keeps its own
/// count of the composing.
#[derive(Debug, Default)]
struct Stats {
    report: crate::logging::Report,
    frames: u32,
    bytes: u64,
    encode: Duration,
    write: Duration,
}

impl Stats {
    fn record(&mut self, bytes: usize, encode: Duration, write: Duration) {
        self.frames += 1;
        self.bytes += bytes as u64;
        self.encode += encode;
        self.write += write;

        let Some(seconds) = self.report.due() else {
            return;
        };
        let frames = f64::from(self.frames);
        let per_frame = |total: Duration| total.as_secs_f64() * 1e3 / frames;
        tracing::debug!(
            fps = frames / seconds,
            kib = self.bytes / u64::from(self.frames) / 1024,
            encode_ms = per_frame(self.encode),
            write_ms = per_frame(self.write),
            "frames sent to the terminal"
        );
        *self = Self {
            report: self.report,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use base64::Engine as _;

    use super::*;
    use crate::protocol::pane::{self, ToClient};

    /// The pixels a frame's escapes carry, decoded the way a terminal would:
    /// every chunk of the transmission, base64'd, and zlib compressed if the
    /// escapes say so.
    fn pixels_in(escapes: &[u8]) -> Vec<u8> {
        use std::io::Read as _;

        let text = std::str::from_utf8(escapes).expect("escapes are ASCII");
        let compressed = text.contains("o=z");
        let mut payload = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").expect("an unterminated escape");
            let (_, chunk) = body[..end].split_once(';').expect("a payload");
            payload.push_str(chunk);
            rest = &body[end + 2..];
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("the payload is base64");
        if !compressed {
            return raw;
        }
        let mut pixels = Vec::new();
        flate2::read::ZlibDecoder::new(raw.as_slice())
            .read_to_end(&mut pixels)
            .expect("the payload is a zlib stream");
        pixels
    }

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
        }
    }

    #[test]
    fn what_a_presenter_writes_is_what_a_terminal_takes() {
        // The whole of a pane's way out: the frame goes to the socket as the
        // bytes the terminal takes, one image holding all of it, and comes back
        // when the pane has them.
        let (pane, terminal) = UnixStream::pair().expect("a socket pair");
        let (events, freed) = calloop::channel::channel();
        let presenter = Presenter::new(events).expect("a presenter");
        presenter.attach(pane, &capabilities());

        let mut frame = Frame::new(160, 160);
        frame.clear([9, 9, 9]);
        let pixels = frame.pixels().to_vec();
        presenter.present(frame);

        let mut terminal = std::io::BufReader::new(terminal);
        let message = pane::read::<_, ToClient>(&mut terminal).expect("a message");
        let ToClient::Frame(escapes) = message else {
            panic!("a pane is sent a frame, not {message:?}");
        };
        assert!(
            escapes.starts_with(b"\x1b[?2026h"),
            "a frame is one synchronized update"
        );
        assert!(escapes.ends_with(b"\x1b[?2026l"));
        assert_eq!(
            escapes
                .windows(b"a=T,f=24".len())
                .filter(|window| *window == b"a=T,f=24")
                .count(),
            1,
            "the whole screen is one image"
        );
        assert!(
            escapes
                .windows(b",s=160,v=160,".len())
                .any(|window| window == b",s=160,v=160,"),
            "at the frame's own size"
        );
        assert!(!escapes.windows(3).any(|window| window == b",c="));
        assert_eq!(pixels_in(&escapes), pixels);

        // The pane has written it, so the frame is the compositor's again.
        presenter.drawn();
        let free = freed.recv().expect("the frame comes back");
        let Event::Free { frame } = free else {
            panic!("{free:?}");
        };
        assert_eq!((frame.width, frame.height), (160, 160));
    }
}
