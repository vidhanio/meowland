//! Handing frames to the terminal from a thread of its own.
//!
//! Composing a frame is cheap; compressing and sending it is not, and it is the
//! terminal that sets the pace once it starts. Doing that on the thread that
//! reads input means every keystroke waits behind whatever the last frame
//! happened to cost - milliseconds, on every frame, for as long as there is
//! something on screen that moves.
//!
//! So the loop composes, works out which tiles changed, copies those, and hands
//! them over. The worker compresses them, escapes them, and writes them. The
//! loop never waits: if the worker is still busy, the frame is *dropped* rather
//! than queued, because a frame that is already out of date is worth less than
//! the newest one, and the tiles that were dropped stay due, so the next frame
//! the worker gets carries everything the terminal has not seen yet.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{Receiver, Sender as MessageSender, channel},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::channel::Sender as EventSender;

use crate::{
    kitty::{self, Encoder, Placement},
    tty::{self, Capabilities, Terminal},
};

/// One frame on its way to the terminal: the pixels of the tiles that changed,
/// laid end to end, and where each of them goes.
#[derive(Debug, Default)]
pub struct Frame {
    /// Tile pixels, in the order the tiles are listed.
    pub pixels: Vec<u8>,
    pub tiles: Vec<Placement>,
}

/// Why terminal presentation stopped.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start the presenter thread")]
    Start(#[source] std::io::Error),
    #[error("could not write terminal output")]
    Output(#[source] std::io::Error),
    #[error("the presenter thread panicked")]
    Panicked,
}

/// A frame, or an escape that has to keep its place among frames: wiping the
/// screen after a resize, or naming the pointer shape.
#[derive(Debug)]
enum Message {
    Configure { shared_memory: bool },
    Frame(Frame),
    Raw(Vec<u8>),
}

/// What the writer tells the main loop after processing a message.
#[derive(Debug)]
pub enum Event {
    /// The one reusable frame is available for the next presentation.
    Ready(Frame),
    /// Terminal output can no longer continue.
    Failed(Error),
}

/// The thread that writes to the terminal.
#[derive(Debug)]
pub struct Presenter {
    /// Held rather than sent through: dropping it ends the worker.
    messages: Option<MessageSender<Message>>,
    /// Recycling both vectors keeps a steady stream from allocating either
    /// pixels or placements per frame.
    free: Option<Frame>,
    handle: Option<JoinHandle<()>>,
    /// Dropped only after [`Presenter::drop`] has joined the writer.
    terminal: Terminal,
}

impl Presenter {
    /// Start the dormant worker. It receives terminal capabilities on
    /// activation.
    pub fn new(terminal: Terminal, events: EventSender<Event>) -> Result<Self, Error> {
        // One frame in flight at a time: a backlog of frames is a backlog of
        // latency, and the newest frame supersedes the ones before it.
        // Frames are bounded by the single recyclable frame below. The channel
        // itself stays unbounded so control escapes are never discarded merely
        // because the worker is writing a frame.
        let (messages, queue) = channel();
        let handle = thread::Builder::new()
            .name("meowland-presenter".into())
            .spawn(move || worker(queue, &events))
            .map_err(Error::Start)?;
        Ok(Self {
            messages: Some(messages),
            free: Some(Frame::default()),
            handle: Some(handle),
            terminal,
        })
    }

    pub const fn capabilities(&self) -> &Capabilities {
        self.terminal.capabilities()
    }

    /// Take over the terminal and configure the writer for its transport.
    pub fn activate(&mut self) -> Result<Capabilities, tty::Error> {
        let capabilities = self.terminal.activate()?.clone();
        if let Some(messages) = &self.messages {
            let _ = messages.send(Message::Configure {
                shared_memory: capabilities.shared_memory,
            });
        }
        Ok(capabilities)
    }

    pub fn refresh(&mut self) -> &Capabilities {
        self.terminal.refresh()
    }

    pub fn clear(&self) {
        self.raw(Terminal::clear());
    }

    /// A recycled frame to fill, if the worker has finished with it.
    ///
    /// `None` means the worker is still busy and this frame is being dropped -
    /// dropping is the point, so it is not an error.
    pub const fn frame(&mut self) -> Option<Frame> {
        self.free.take()
    }

    pub const fn is_ready(&self) -> bool {
        self.free.is_some()
    }

    /// Make a completed or unsent frame available for reuse.
    pub fn recycle(&mut self, frame: Frame) {
        debug_assert!(self.free.is_none());
        self.free = Some(frame);
    }

    /// Hand over a frame, or take it back if the worker has stopped.
    ///
    /// The caller keeps the tiles it could not hand over: they are still due,
    /// and the next frame carries them along with its own.
    pub fn present(&self, frame: Frame) -> Result<(), Frame> {
        let Some(messages) = &self.messages else {
            return Err(frame);
        };
        messages
            .send(Message::Frame(frame))
            .map_err(|error| match error.0 {
                Message::Frame(frame) => frame,
                Message::Configure { .. } | Message::Raw(_) => unreachable!("sent a frame"),
            })
    }

    /// Show an escape in its turn, after the frames already handed over.
    pub fn raw(&self, bytes: Vec<u8>) {
        if let Some(messages) = &self.messages {
            let _ = messages.send(Message::Raw(bytes));
        }
    }

    /// Write everything already handed over, and stop the worker.
    ///
    /// Called before the terminal is dropped, so that the escapes that undo
    /// what the compositor did to it are the last thing written.
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

/// Keep failures and panics on the worker observable to the event loop.
fn worker(queue: Receiver<Message>, events: &EventSender<Event>) {
    struct Cleanup;

    impl Drop for Cleanup {
        fn drop(&mut self) {
            kitty::discard_shared_memory();
        }
    }

    let _cleanup = Cleanup;
    let result = catch_unwind(AssertUnwindSafe(|| run(queue, events)));
    let failure = match result {
        Ok(Ok(())) => return,
        Ok(Err(err)) => Error::Output(err),
        Err(_) => Error::Panicked,
    };
    let _ = events.send(Event::Failed(failure));
}

/// The worker: encode frames in the order they arrive and write them out.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the worker outlives whoever started it, so it owns its ends of the channels rather than borrowing them"
)]
fn run(queue: Receiver<Message>, events: &EventSender<Event>) -> std::io::Result<()> {
    let mut encoder = Encoder::default();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    while let Ok(message) = queue.recv() {
        let mut frame = match message {
            Message::Configure { shared_memory } => {
                encoder.shared_memory = shared_memory;
                continue;
            }
            Message::Frame(frame) => frame,
            Message::Raw(bytes) => {
                write(&bytes)?;
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
        write(&out)?;
        let written = phase.elapsed();

        frame.pixels.clear();
        frame.tiles.clear();
        stats.record(tiles, out.len(), spent_encoding, written);
        if events.send(Event::Ready(frame)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

fn write(bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// What the worker did in the last second, logged so that a frame rate can be
/// attributed: this thread's time is the terminal's and the compressor's.
#[derive(Debug, Default)]
struct Stats {
    since: Option<Instant>,
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

        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        let elapsed = now - since;
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let frames = f64::from(self.frames);
        let per_frame = |total: Duration| total.as_secs_f64() * 1e3 / frames;
        tracing::debug!(
            fps = frames / elapsed.as_secs_f64(),
            tiles = self.tiles / u64::from(self.frames),
            kib = self.bytes / u64::from(self.frames) / 1024,
            encode_ms = per_frame(self.encode),
            write_ms = per_frame(self.write),
            "frames sent to the terminal"
        );
        *self = Self {
            since: Some(now),
            ..Self::default()
        };
    }
}

/// The pointer shape, as the terminal wants to hear about it.
pub fn pointer_shape_bytes(shape: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    kitty::set_pointer_shape(&mut out, shape);
    out
}
