//! Handing frames to the attached terminal from a thread of its own.
//!
//! Composing a frame is cheap. Compressing it and writing it is not, and the
//! terminal sets the pace once it starts. On the thread that reads input, every
//! keystroke would wait behind the last frame, for milliseconds on every frame,
//! for as long as something on the screen moves.
//!
//! The loop composes a frame, works out which tiles changed, copies those tiles
//! and hands them over. The worker compresses them, escapes them and writes
//! them to the terminal that is attached at the time. The loop never waits. If
//! the worker is still busy, the frame is dropped rather than queued. A frame
//! that is already out of date is worth less than the newest one. The tiles
//! that were dropped stay due, so the next frame that the worker gets carries
//! everything that the terminal has not seen.
//!
//! A server has no terminal of its own, so this is where attaching is felt. The
//! worker writes into a socket while a terminal is attached, and holds nothing
//! in between. What the terminal does with the bytes, which is to write them
//! and to say when it has, belongs to the terminal side (`display`).

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
    display::{self, ToClient},
    kitty::{self, Encoder, Placement},
    tty,
};

/// One frame on its way to the terminal: the pixels of the tiles that changed,
/// laid end to end, and the position of each tile.
#[derive(Debug, Default)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub tiles: Vec<Placement>,
}

/// A frame, or an escape that must keep its place among frames, such as a
/// screen wipe after a resize or the pointer shape.
#[derive(Debug)]
enum Message {
    Attach(Option<UnixStream>),
    Configure {
        shared_memory: bool,
    },
    Tell(ToClient),
    Frame(Frame),
    /// The terminal wrote the frame that it was sent, so the next frame may go.
    Drawn,
    Raw(Vec<u8>),
}

/// What the worker reports to the event loop.
#[derive(Debug)]
pub enum Event {
    /// The one reusable frame is free for the next presentation.
    Ready(Frame),
    Failed(Error),
}

/// The thread that writes to whichever terminal is attached.
#[derive(Debug)]
pub struct Presenter {
    /// Held, not sent through: dropping the sender ends the worker.
    messages: Option<MessageSender<Message>>,
    /// Recycling both vectors keeps a steady stream of frames from allocating
    /// pixels or placements for each frame.
    free: Option<Frame>,
    handle: Option<JoinHandle<()>>,
}

impl Presenter {
    /// Start the worker, which writes nothing until a terminal attaches.
    pub fn new(events: EventSender<Event>) -> Result<Self, Error> {
        // One frame is in flight at a time: a backlog of frames is a backlog of
        // latency, and the newest frame replaces the ones before it. The single
        // recyclable frame below bounds the frames. The channel stays
        // unbounded, so a control escape is not discarded because the worker is
        // writing a frame.
        let (messages, queue) = channel();
        let handle = thread::Builder::new()
            .name("meowland-presenter".into())
            .spawn(move || worker(queue, &events))?;
        Ok(Self {
            messages: Some(messages),
            free: Some(Frame::default()),
            handle: Some(handle),
        })
    }

    pub fn attach(&self, terminal: UnixStream, shared_memory: bool) {
        self.send(Message::Configure { shared_memory });
        self.send(Message::Attach(Some(terminal)));
    }

    pub fn detach(&self, reason: ToClient) {
        self.send(Message::Tell(reason));
        self.send(Message::Attach(None));
    }

    pub fn clear(&self) {
        self.raw(tty::Terminal::clear());
    }

    /// A recycled frame to fill, if the worker has finished with it.
    ///
    /// `None` means that the worker is still busy and that this frame is
    /// dropped. Dropping the frame is intended, so it is not an error.
    pub const fn frame(&mut self) -> Option<Frame> {
        self.free.take()
    }

    pub const fn is_ready(&self) -> bool {
        self.free.is_some()
    }

    pub fn recycle(&mut self, frame: Frame) {
        debug_assert!(self.free.is_none());
        self.free = Some(frame);
    }

    /// Hand over a frame, or take it back if the worker has stopped.
    ///
    /// The caller keeps the tiles that it could not hand over. They are still
    /// due, and the next frame carries them with its own.
    pub fn present(&self, frame: Frame) -> Result<(), Frame> {
        let Some(messages) = &self.messages else {
            return Err(frame);
        };
        messages
            .send(Message::Frame(frame))
            .map_err(|error| match error.0 {
                Message::Frame(frame) => frame,
                // The message that failed was the frame that was handed over,
                // and nothing else is a frame.
                _ => unreachable!("sent a frame"),
            })
    }

    /// The terminal wrote the frame that it was sent, so the next frame may
    /// follow.
    ///
    /// The server keeps one frame on its way at a time. While this frame is
    /// written, the loop drops the frames that it would otherwise queue. The
    /// tiles that they carried stay due for the frame that goes after.
    pub fn drawn(&self) {
        self.send(Message::Drawn);
    }

    /// Show an escape in its turn, after the frames already handed over.
    pub fn raw(&self, bytes: Vec<u8>) {
        self.send(Message::Raw(bytes));
    }

    fn send(&self, message: Message) {
        if let Some(messages) = &self.messages {
            let _ = messages.send(message);
        }
    }

    /// Write everything already handed over, then stop the worker. The server
    /// calls this before it drops the terminal, so that the escapes that undo
    /// what the compositor did to the terminal are written last.
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
    reason = "the worker outlives whoever started it, so it owns its ends of the channels rather than borrowing them"
)]
fn run(queue: Receiver<Message>, events: &EventSender<Event>) -> std::io::Result<()> {
    let mut encoder = Encoder::new();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    let mut terminal: Option<UnixStream> = None;
    // The frame that the terminal was sent and has not said that it wrote.
    // Nothing else can be sent until the terminal says so, because any frame
    // that the loop handed over in the meantime would show a screen that no
    // longer looks like that.
    let mut in_flight: Option<Frame> = None;
    while let Ok(message) = queue.recv() {
        let mut frame = match message {
            Message::Attach(attached) => {
                terminal = attached;
                if terminal.is_none() {
                    recycle(&mut in_flight, events)?;
                }
                continue;
            }
            Message::Configure { shared_memory } => {
                encoder.shared_memory = shared_memory;
                continue;
            }
            Message::Tell(message) => {
                tell(&mut terminal, message);
                continue;
            }
            Message::Drawn => {
                recycle(&mut in_flight, events)?;
                continue;
            }
            Message::Frame(frame) => frame,
            Message::Raw(bytes) => {
                write(&mut terminal, bytes)?;
                continue;
            }
        };
        let tiles = frame.tiles.len();

        let phase = Instant::now();
        out.clear();
        Encoder::begin_frame(&mut out);
        let mut offset = 0;
        for placement in &frame.tiles {
            let length = placement.bytes();
            let tile = &frame.pixels[offset..offset + length];
            offset += length;
            encoder.transmit_and_place(&mut out, tile, *placement);
        }
        encoder.end_frame(&mut out);
        let spent_encoding = phase.elapsed();

        let phase = Instant::now();
        let written = match write_frame(&mut terminal, &out) {
            Ok(()) => phase.elapsed(),
            Err(error) => {
                // A write to the socket fails when the terminal on the other
                // end has gone, because it closed, or was killed, or its end of
                // the pty closed. This thread does not stop for that. The loop
                // hears the same thing from its reader and detaches, and the
                // server runs until another terminal attaches.
                tracing::debug!(%error, "the attached terminal is gone");
                terminal = None;
                Duration::ZERO
            }
        };
        frame.pixels.clear();
        frame.tiles.clear();
        if terminal.is_none() {
            // The frame was not written, and the tiles are still due, so hand
            // the frame back for the next frame to carry.
            if events.send(Event::Ready(frame)).is_err() {
                return Ok(());
            }
            continue;
        }

        stats.record(tiles, out.len(), spent_encoding, written);
        in_flight = Some(frame);
    }
    Ok(())
}

fn recycle(in_flight: &mut Option<Frame>, events: &EventSender<Event>) -> std::io::Result<()> {
    let Some(frame) = in_flight.take() else {
        return Ok(());
    };
    events
        .send(Event::Ready(frame))
        .map_err(|_| std::io::Error::other("the event loop is gone"))
}

fn write(terminal: &mut Option<UnixStream>, bytes: Vec<u8>) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    display::write_to(terminal, display::encode_client(ToClient::Bytes(bytes)))
}

/// A frame is written as a frame, not as an escape.
///
/// It is the one message that the terminal answers, and the bytes are written
/// in place because the frame is recycled afterwards.
fn write_frame(terminal: &mut Option<UnixStream>, bytes: &[u8]) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    display::write_frame(terminal, bytes)
}

/// Something about the attachment itself, which is never a frame.
fn tell(terminal: &mut Option<UnixStream>, message: ToClient) {
    if let Some(terminal) = terminal {
        let _ = display::write_to(terminal, display::encode_client(message));
    }
}

/// What the worker did in the last second, logged so that a frame rate is
/// attributed. The time of this thread is spent in the terminal and in the
/// compressor.
#[derive(Debug, Default)]
struct Stats {
    report: crate::logging::Report,
    frames: u32,
    tiles: u64,
    bytes: u64,
    encode: Duration,
    write: Duration,
}

impl Stats {
    fn record(&mut self, tiles: usize, bytes: usize, encode: Duration, write: Duration) {
        self.frames += 1;
        self.tiles += tiles as u64;
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
            tiles = self.tiles / u64::from(self.frames),
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

pub fn pointer_shape_bytes(shape: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    kitty::set_pointer_shape(&mut out, shape);
    out
}
