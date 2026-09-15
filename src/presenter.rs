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
    sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::kitty::{self, Encoder, Placement};

/// One frame on its way to the terminal: the pixels of the tiles that changed,
/// laid end to end, and where each of them goes.
#[derive(Debug)]
pub struct Frame {
    /// Tile pixels, in the order the tiles are listed.
    pub pixels: Vec<u8>,
    pub tiles: Vec<Placement>,
}

/// A frame, or an escape that has to keep its place among frames: wiping the
/// screen after a resize, or naming the pointer shape.
#[derive(Debug)]
enum Message {
    Frame(Frame),
    Raw(Vec<u8>),
}

/// The thread that writes to the terminal.
#[derive(Debug)]
pub struct Presenter {
    /// Held rather than sent through: dropping it ends the worker.
    messages: Option<SyncSender<Message>>,
    /// Buffers the worker has finished with, so that a steady stream of frames
    /// does not allocate one per frame.
    recycled: Receiver<Vec<u8>>,
    free: Vec<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

impl Presenter {
    /// Start the worker, telling it whether the terminal reads tiles out of
    /// shared memory rather than off the pty.
    pub fn new(shared_memory: bool) -> Self {
        // One frame in flight at a time: a backlog of frames is a backlog of
        // latency, and the newest frame supersedes the ones before it.
        let (messages, queue) = sync_channel::<Message>(1);
        let (recycle, recycled) = std::sync::mpsc::channel();
        let handle = thread::Builder::new()
            .name("meowland-presenter".into())
            .spawn(move || run(queue, recycle, shared_memory))
            .expect("the presenter thread could not be started");
        Self {
            messages: Some(messages),
            recycled,
            // One to start with: the worker only returns buffers it has been
            // given, so a pool that begins empty has nothing to hand over.
            free: vec![Vec::new()],
            handle: Some(handle),
        }
    }

    /// A buffer to fill with the tiles of a frame, if there is one to be had.
    ///
    /// `None` means the worker is still busy and this frame is being dropped -
    /// dropping is the point, so it is not an error.
    pub fn buffer(&mut self) -> Option<Vec<u8>> {
        self.free.extend(self.recycled.try_iter());
        self.free.pop()
    }

    /// Give back a buffer that was taken but not filled.
    pub fn reuse(&mut self, buffer: Vec<u8>) {
        self.free.push(buffer);
    }

    /// Hand over a frame, or take it back if the worker is not ready for one.
    ///
    /// The caller keeps the tiles it could not hand over: they are still due,
    /// and the next frame carries them along with its own.
    pub fn present(&self, frame: Frame) -> Result<(), Frame> {
        let Some(messages) = &self.messages else {
            return Ok(());
        };
        match messages.try_send(Message::Frame(frame)) {
            // A frame the worker is too busy for is dropped, and goes back to
            // the caller to be copied again from a newer one later.
            Err(TrySendError::Full(Message::Frame(frame))) => Err(frame),
            // Handed over, or the worker is gone: either way there is nothing
            // left to do with it.
            _ => Ok(()),
        }
    }

    /// Show an escape in its turn, after the frames already handed over.
    pub fn raw(&self, bytes: Vec<u8>) {
        if let Some(messages) = &self.messages {
            let _ = messages.try_send(Message::Raw(bytes));
        }
    }

    /// Write everything already handed over, and stop the worker.
    ///
    /// Called before the terminal is dropped, so that the escapes that undo
    /// what the compositor did to it are the last thing written.
    pub fn finish(&mut self) {
        self.messages = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker: encode frames in the order they arrive and write them out.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the worker outlives whoever started it, so it owns its ends of the channels rather than borrowing them"
)]
fn run(queue: Receiver<Message>, recycle: Sender<Vec<u8>>, shared_memory: bool) {
    let mut encoder = Encoder::default();
    encoder.shared_memory = shared_memory;
    let mut out = Vec::new();
    let mut stats = Stats::default();
    while let Ok(message) = queue.recv() {
        let frame = match message {
            Message::Frame(frame) => frame,
            Message::Raw(bytes) => {
                if let Err(err) = write(&bytes) {
                    tracing::warn!(?err, "could not hand an escape to the terminal");
                }
                continue;
            }
        };
        let tiles = frame.tiles.len();

        let phase = Instant::now();
        out.clear();
        Encoder::begin_frame(&mut out);
        let mut offset = 0;
        for placement in &frame.tiles {
            let length = placement.width as usize * placement.height as usize * 4;
            let tile = &frame.pixels[offset..offset + length];
            offset += length;
            encoder.transmit_and_place(&mut out, tile, *placement);
        }
        encoder.end_frame(&mut out);
        let spent_encoding = phase.elapsed();

        let phase = Instant::now();
        let result = write(&out);
        let written = phase.elapsed();
        if let Err(err) = result {
            tracing::warn!(?err, "could not hand a frame to the terminal");
        }

        // The buffer goes back for the next frame to be filled in.
        let _ = recycle.send(frame.pixels);
        stats.record(tiles, out.len(), spent_encoding, written);
    }
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
        if elapsed < Duration::from_secs(1) || self.frames == 0 {
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
