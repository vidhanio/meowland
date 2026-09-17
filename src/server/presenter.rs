//! Per-pane frame encoding and output thread.

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
    kitty::{self, Encoder, Placement},
    protocol::pane::{self, ToClient},
};

/// One frame on its way to the terminal: the pixels of the tiles that changed,
/// laid end to end, and the position of each tile.
#[derive(Debug, Default)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub tiles: Vec<Placement>,
}

#[derive(Debug)]
enum Message {
    Attach(Option<UnixStream>),
    Configure { shared_memory: bool },
    Tell(ToClient),
    Frame(Frame),
    Drawn,
    Raw(Vec<u8>),
}

#[derive(Debug)]
pub enum Event {
    Ready(Frame),
    Failed(Error),
}

#[derive(Debug)]
pub struct Presenter {
    messages: Option<MessageSender<Message>>,
    free: Option<Frame>,
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
        self.raw(kitty::clear());
    }

    /// Returns `None` while a frame is in flight.
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

    /// Returns the frame if the worker has stopped.
    pub fn present(&self, frame: Frame) -> Result<(), Frame> {
        let Some(messages) = &self.messages else {
            return Err(frame);
        };
        messages
            .send(Message::Frame(frame))
            .map_err(|error| match error.0 {
                Message::Frame(frame) => frame,
                _ => unreachable!("sent a frame"),
            })
    }

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
    reason = "the worker outlives whoever started it, so it owns its ends of the channels rather than borrowing them"
)]
fn run(queue: Receiver<Message>, events: &EventSender<Event>) -> std::io::Result<()> {
    let mut encoder = Encoder::new();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    let mut terminal: Option<UnixStream> = None;
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
                // A disconnected pane does not stop the worker.
                tracing::debug!(%error, "the attached terminal is gone");
                terminal = None;
                Duration::ZERO
            }
        };
        frame.pixels.clear();
        frame.tiles.clear();
        if terminal.is_none() {
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
    pane::write_to(terminal, pane::encode_client(ToClient::Bytes(bytes)))
}

fn write_frame(terminal: &mut Option<UnixStream>, bytes: &[u8]) -> std::io::Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };
    pane::write_frame(terminal, bytes)
}

fn tell(terminal: &mut Option<UnixStream>, message: ToClient) {
    if let Some(terminal) = terminal {
        let _ = pane::write_to(terminal, pane::encode_client(message));
    }
}

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
