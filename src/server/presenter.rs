//! Per-pane frame encoding and output thread.
//!
//! The compositor draws a pane's whole screen; this thread puts it on the
//! terminal and writes it to the pane's socket. A frame goes whole when the
//! terminal has none, and as patches over the frame it already has when only
//! part of the screen changed: a terminal that takes a screen's worth of pixels
//! per frame drops frames it could have shown. It runs on a thread of its own,
//! so a slow terminal holds up only its own pane.

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
    kitty::{self, Encoder, FIRST_PATCH_ID, ImageId, Patch},
    protocol::pane::{self, Capabilities},
    render::{BYTES, Frame, Rect},
};

/// One tile of a pane's screen, in cells.
///
/// A tile is a whole number of cells in both directions, which is what makes a
/// patch foolproof: it is placed in the cell its top-left pixel is in, and that
/// is the pixel it holds.
const TILE: (u32, u32) = (8, 2);

/// The most tiles one frame may carry. Past this the screen goes whole: a frame
/// this broken up costs more in escapes than the pixels it saves.
const MAXIMUM_PATCHES: usize = 32;

/// The most tiles the terminal may be left holding. Each one is an image it
/// keeps and a draw it makes, so the next frame is whole once they would be
/// more than this, and a whole frame clears them.
const MAXIMUM_LIVE: usize = 64;

#[derive(Debug)]
enum Message {
    Attach(Option<UnixStream>),
    /// What the terminal this pane is in can do.
    Capabilities(Config),
    /// Let go of the pane, and say why.
    Detached(String),
    /// Wipe the screen, every image with it.
    Clear,
    Frame(Frame),
    Drawn,
    Raw(Vec<u8>),
}

/// What the presenter needs to know about the terminal it writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Config {
    /// One character cell, in pixels.
    cell: (u32, u32),
    /// Whether the terminal reads frames out of shared memory, which keeps
    /// their pixels off the pty.
    shared_memory: bool,
    /// Whether a rectangle of a frame may be sent as a patch.
    ///
    /// A patch is placed in the cell its top-left pixel is in, so this takes
    /// the cell size the terminal itself reported: a cell derived from a pixel
    /// size divides wrongly, and a patch placed a pixel out is a screen with
    /// the wrong pixels on it.
    patches: bool,
}

impl Config {
    fn of(capabilities: &Capabilities) -> Self {
        Self {
            cell: (capabilities.cell.0.max(1), capabilities.cell.1.max(1)),
            shared_memory: capabilities.shared_memory,
            patches: capabilities.patches && capabilities.cell.0 > 0 && capabilities.cell.1 > 0,
        }
    }
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

    /// What the terminal this pane is in can do, and how large its cells are.
    pub fn configure(&self, capabilities: &Capabilities) {
        self.send(Message::Capabilities(Config::of(capabilities)));
    }

    pub fn detach(&self, reason: &str) {
        self.send(Message::Detached(reason.to_owned()));
        self.send(Message::Attach(None));
    }

    /// Wipe the screen the pane is drawn on, and every image on it.
    pub fn clear(&self) {
        self.send(Message::Clear);
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
    let mut config = Config::default();
    let mut out = Vec::new();
    let mut patch = Vec::new();
    let mut stats = Stats::default();
    let mut terminal: Option<UnixStream> = None;
    let mut in_flight: Option<Frame> = None;
    let mut screen = Screen::default();
    while let Ok(message) = queue.recv() {
        let frame = match message {
            Message::Attach(attached) => {
                terminal = attached;
                if terminal.is_none() {
                    free(&mut in_flight, events)?;
                }
                // Whatever terminal this is, it has yet to be sent anything.
                screen.reset();
                continue;
            }
            Message::Capabilities(what) => {
                config = what;
                // A whole frame goes through a shared memory object when the
                // terminal reads those, which keeps its pixels off the pty and
                // out of the compressor. A patch always goes the pty's way.
                encoder.shared_memory = what.shared_memory;
                continue;
            }
            Message::Detached(reason) => {
                write_detached(&mut terminal, &reason)?;
                continue;
            }
            Message::Clear => {
                screen.reset();
                write(&mut terminal, &kitty::clear())?;
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

        let plan = screen.plan(&frame, &config);
        if matches!(plan, Send::Nothing) {
            // The terminal shows this frame already, so this one costs nothing
            // and the compositor may draw the next at once.
            stats.skipped();
            if events.send(Event::Free { frame }).is_err() {
                return Ok(());
            }
            continue;
        }

        let phase = Instant::now();
        out.clear();
        Encoder::begin_frame(&mut out);
        let patches = match &plan {
            // Handled above: the frame costs nothing.
            Send::Nothing => 0,
            Send::Whole { delete } => {
                kitty::delete_images(&mut out, delete);
                encoder.transmit(&mut out, frame.pixels(), (frame.width, frame.height));
                1
            }
            Send::Patches { write } => {
                for plan in write {
                    cut(&frame, plan.rect, &mut patch);
                    encoder.transmit_patch(
                        &mut out,
                        Patch {
                            id: plan.id,
                            cell: plan.cell,
                            size: (plan.rect.width, plan.rect.height),
                            pixels: &patch,
                        },
                    );
                }
                write.len() as u64
            }
        };
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
            screen.reset();
            if events.send(Event::Free { frame }).is_err() {
                return Ok(());
            }
            continue;
        }

        stats.record(patches, out.len(), spent_encoding, written);
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

/// The frame the terminal shows, and the tiles that were sent over it.
///
/// A pane's screen is one image of the whole frame. After that a frame goes as
/// tiles: images of the parts of the screen that changed, placed over the image
/// the terminal holds. So what the terminal shows is that image with the tiles
/// on top, and a whole frame, which replaces the image, takes them all with it.
#[derive(Debug, Default)]
struct Screen {
    /// The pixels the terminal was last sent.
    previous: Vec<u8>,
    /// The grid those pixels were sent on.
    grid: Grid,
    /// Whether the terminal's whole-screen image is those pixels. It is not
    /// until a frame goes whole: an attached terminal, a resize, or a wipe
    /// leaves a screen with nothing on it.
    whole: bool,
    /// Which tiles the terminal holds, by index.
    placed: Vec<bool>,
}

impl Screen {
    /// Forget what the terminal shows: the next frame is a whole one. Whatever
    /// tiles it holds are gone with it, so nothing is left to delete.
    fn reset(&mut self) {
        self.whole = false;
        self.placed.clear();
    }

    /// What to write for this frame, taking it as what the terminal will show.
    fn plan(&mut self, frame: &Frame, config: &Config) -> Send {
        let grid = Grid::of(frame, config.cell);
        if !self.whole || grid != self.grid {
            return self.whole(frame, grid);
        }
        let changed = changed(&grid, &self.previous, frame.pixels());
        if changed.is_empty() {
            return Send::Nothing;
        }
        let held = self.placed.iter().filter(|placed| **placed).count();
        // The pixels the tiles would carry, against the pixels a whole frame
        // would: past that, the whole screen is the cheaper way to say it.
        let covered = changed.len() as u64 * u64::from(grid.tile.0) * u64::from(grid.tile.1);
        let screen = u64::from(grid.screen.0) * u64::from(grid.screen.1);
        if !config.patches
            || changed.len() > MAXIMUM_PATCHES
            || held + changed.len() > MAXIMUM_LIVE
            || covered > screen
        {
            return self.whole(frame, grid);
        }
        for index in &changed {
            self.placed[*index as usize] = true;
        }
        self.previous.copy_from_slice(frame.pixels());
        Send::Patches {
            write: changed
                .into_iter()
                .map(|index| Plan {
                    id: Grid::id(index),
                    cell: grid.cell(index),
                    rect: grid.rect(index),
                })
                .collect(),
        }
    }

    /// Send the whole screen: the image replaces every tile with it.
    fn whole(&mut self, frame: &Frame, grid: Grid) -> Send {
        let delete = Grid::held(&self.placed);
        self.grid = grid;
        self.placed = vec![false; grid.count() as usize];
        self.previous.clear();
        self.previous.extend_from_slice(frame.pixels());
        self.whole = true;
        Send::Whole { delete }
    }
}

/// What the presenter writes for one frame.
#[derive(Debug)]
enum Send {
    /// Nothing: the terminal already shows this frame.
    Nothing,
    /// The whole screen, as one image. `delete` are the tiles it replaces.
    Whole { delete: Vec<ImageId> },
    /// Tiles of it, over the image the terminal holds.
    Patches { write: Vec<Plan> },
}

/// One tile to write: which image, which cell it goes in, and which pixels.
#[derive(Debug, Clone, Copy)]
struct Plan {
    id: ImageId,
    cell: (u32, u32),
    rect: Rect,
}

/// Where a frame sits in the pane's tile grid.
///
/// A tile is a whole number of cells in both directions and the grid begins at
/// the screen's top left, so the cell a tile is sent to is the cell its
/// top-left pixel is in. Nothing here has to round anything to a cell: a patch
/// cannot land anywhere but on the pixels it holds. The tiles at the right and
/// bottom edges are what is left of the screen, which need not be a whole tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Grid {
    /// The screen, in pixels.
    screen: (u32, u32),
    /// One tile, in pixels.
    tile: (u32, u32),
    /// How many tiles across.
    columns: u32,
}

impl Grid {
    const fn of(frame: &Frame, cell: (u32, u32)) -> Self {
        let tile = (cell.0 * TILE.0, cell.1 * TILE.1);
        Self {
            screen: (frame.width, frame.height),
            tile,
            columns: frame.width.div_ceil(tile.0),
        }
    }

    const fn rows(&self) -> u32 {
        self.screen.1.div_ceil(self.tile.1)
    }

    const fn count(&self) -> u32 {
        self.columns * self.rows()
    }

    /// The pixels one tile holds.
    fn rect(&self, index: u32) -> Rect {
        let (column, row) = (index % self.columns, index / self.columns);
        let (x, y) = (column * self.tile.0, row * self.tile.1);
        Rect::new(
            x as i32,
            y as i32,
            self.tile.0.min(self.screen.0 - x),
            self.tile.1.min(self.screen.1 - y),
        )
    }

    /// The cell one tile's top-left pixel is in.
    const fn cell(&self, index: u32) -> (u32, u32) {
        (
            (index % self.columns) * TILE.0,
            (index / self.columns) * TILE.1,
        )
    }

    /// The image one tile is sent under. A tile keeps its image while the grid
    /// does, so sending a tile again replaces the one the terminal holds.
    const fn id(index: u32) -> ImageId {
        ImageId::new(FIRST_PATCH_ID + index)
    }

    /// The images of the tiles the terminal holds.
    fn held(placed: &[bool]) -> Vec<ImageId> {
        placed
            .iter()
            .enumerate()
            .filter(|(_, placed)| **placed)
            .map(|(index, _)| Self::id(index as u32))
            .collect()
    }
}

/// The tiles of `frame` whose pixels differ from `previous`.
///
/// A row the frame did not change costs one comparison and no more; the tiles
/// are looked at only in the rows that changed. Finding one tile past
/// `MAXIMUM_PATCHES` is enough: the frame goes whole then, and the rest of the
/// screen does not have to be looked at.
fn changed(grid: &Grid, previous: &[u8], frame: &[u8]) -> Vec<u32> {
    let stride = grid.screen.0 as usize * BYTES;
    let mut changed: Vec<u32> = Vec::new();
    for row in 0..grid.screen.1 {
        let at = row as usize * stride;
        if frame[at..at + stride] == previous[at..at + stride] {
            continue;
        }
        for column in 0..grid.columns {
            let x = column * grid.tile.0;
            let end = (x + grid.tile.0).min(grid.screen.0);
            let (from, to) = (at + x as usize * BYTES, at + end as usize * BYTES);
            if frame[from..to] == previous[from..to] {
                continue;
            }
            let index = (row / grid.tile.1) * grid.columns + column;
            if !changed.contains(&index) {
                changed.push(index);
                if changed.len() > MAXIMUM_PATCHES {
                    return changed;
                }
            }
        }
    }
    changed
}

/// Copy one tile out of a frame, rows end to end.
fn cut(frame: &Frame, rect: Rect, out: &mut Vec<u8>) {
    let stride = frame.width as usize * BYTES;
    let length = rect.width as usize * BYTES;
    out.clear();
    out.reserve(rect.height as usize * length);
    for line in 0..rect.height as usize {
        let at = (rect.y as usize + line) * stride + rect.x as usize * BYTES;
        out.extend_from_slice(&frame.pixels()[at..at + length]);
    }
}

/// What the frames of one second cost this thread, logged so that a slow
/// terminal is attributed and not guessed at. The compositor keeps its own
/// count of the composing.
#[derive(Debug, Default)]
struct Stats {
    report: crate::logging::Report,
    frames: u32,
    skipped: u32,
    patches: u64,
    bytes: u64,
    encode: Duration,
    write: Duration,
}

impl Stats {
    /// A frame the terminal was sent, and what it took.
    fn record(&mut self, patches: u64, bytes: usize, encode: Duration, write: Duration) {
        self.frames += 1;
        self.patches += patches;
        self.bytes += bytes as u64;
        self.encode += encode;
        self.write += write;
        self.due();
    }

    /// A frame the terminal already showed, which cost nothing.
    fn skipped(&mut self) {
        self.skipped += 1;
        self.due();
    }

    fn due(&mut self) {
        let Some(seconds) = self.report.due() else {
            return;
        };
        let frames = f64::from(self.frames);
        let per_frame = |total: Duration| {
            if self.frames == 0 {
                0.0
            } else {
                total.as_secs_f64() * 1e3 / frames
            }
        };
        tracing::debug!(
            fps = frames / seconds,
            kib = if self.frames == 0 {
                0.0
            } else {
                self.bytes as f64 / frames / 1024.0
            },
            patches = if self.frames == 0 {
                0.0
            } else {
                self.patches as f64 / frames
            },
            skipped = self.skipped,
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
    use std::{
        io::Read as _,
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };

    use base64::Engine as _;
    use rustix::event::{PollFd, PollFlags, Timespec};

    use super::*;
    use crate::{
        protocol::pane::{self, ToClient},
        render::{Image, SourceFormat},
    };

    /// What a terminal ends up showing: the images it was sent, each where it
    /// was placed, drawn in the order the protocol draws them.
    ///
    /// A terminal handed a bad patch shows the wrong pixels, so this is what a
    /// pane is held to: nothing here asks the terminal what it thinks.
    #[derive(Debug)]
    struct Terminal {
        cell: (u32, u32),
        width: u32,
        height: u32,
        /// The cursor, in cells.
        cursor: (u32, u32),
        placed: Vec<Placed>,
    }

    #[derive(Debug)]
    struct Placed {
        id: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    }

    impl Terminal {
        fn new(width: u32, height: u32, cell: (u32, u32)) -> Self {
            Self {
                cell,
                width,
                height,
                cursor: (0, 0),
                placed: Vec::new(),
            }
        }

        /// Take a pane's escapes the way a terminal does: the cursor moves, the
        /// images are transmitted and placed, and the deletes take theirs away.
        fn write(&mut self, escapes: &[u8]) {
            let text = std::str::from_utf8(escapes).expect("escapes are ASCII");
            let mut rest = text;
            while !rest.is_empty() {
                if let Some(body) = rest.strip_prefix("\x1b[") {
                    let end = body
                        .find(|c: char| ('\u{40}'..='\u{7e}').contains(&c))
                        .expect("a control sequence ends in a final byte");
                    let params = &body[..end];
                    let final_byte = body.as_bytes()[end];
                    rest = &body[end + 1..];
                    // Everything else here is a private mode, such as the
                    // synchronized update the pane wraps a frame in.
                    if final_byte == b'H' && !params.starts_with('?') {
                        let mut fields = params
                            .split(';')
                            .map(|field| field.parse::<u32>().unwrap_or(1));
                        let row = fields.next().unwrap_or(1);
                        let column = fields.next().unwrap_or(1);
                        self.cursor = (column - 1, row - 1);
                    }
                    continue;
                }
                if let Some(body) = rest.strip_prefix("\x1b_G") {
                    let end = body.find("\x1b\\").expect("an unterminated escape");
                    let (control, payload) = body[..end].split_once(';').expect("a payload");
                    let control = control.to_owned();
                    let mut payload = payload.to_owned();
                    let mut more = control.contains("m=1");
                    rest = &body[end + 2..];
                    // A transmission comes in chunks of its own: the first
                    // escape carries the control data, the rest payload.
                    while more {
                        let body = rest
                            .strip_prefix("\x1b_G")
                            .expect("the next chunk of a transmission");
                        let end = body.find("\x1b\\").expect("an unterminated escape");
                        let (next, chunk) = body[..end].split_once(';').expect("a payload");
                        payload.push_str(chunk);
                        more = next.contains("m=1");
                        rest = &body[end + 2..];
                    }
                    self.command(&control, &payload);
                    continue;
                }
                rest = &rest[1..];
            }
        }

        fn command(&mut self, control: &str, payload: &str) {
            match field(control, "a") {
                Some("T") => {
                    assert_eq!(field(control, "f"), Some("24"), "only RGB is sent");
                    let id: u32 = field(control, "i").unwrap().parse().unwrap();
                    let width: u32 = field(control, "s").unwrap().parse().unwrap();
                    let height: u32 = field(control, "v").unwrap().parse().unwrap();
                    let compressed = field(control, "o") == Some("z");
                    let pixels = decode(payload, compressed);
                    assert_eq!(
                        pixels.len(),
                        width as usize * height as usize * BYTES,
                        "the pixels are the size the escape says"
                    );
                    let (column, row) = self.cursor;
                    self.placed.retain(|image| image.id != id);
                    self.placed.push(Placed {
                        id,
                        x: column * self.cell.0,
                        y: row * self.cell.1,
                        width,
                        height,
                        pixels,
                    });
                }
                Some("d") => {
                    if field(control, "d") == Some("A") {
                        self.placed.clear();
                    } else {
                        let id: u32 = field(control, "i").unwrap().parse().unwrap();
                        self.placed.retain(|image| image.id != id);
                    }
                }
                other => panic!("unexpected action {other:?} in {control:?}"),
            }
        }

        /// The pixels on the screen, with every image drawn in id order: the
        /// same z-index, so the lower id is the lower image.
        fn shown(&self) -> Vec<u8> {
            let mut screen = vec![0u8; self.width as usize * self.height as usize * BYTES];
            let mut placed = self.placed.iter().collect::<Vec<_>>();
            placed.sort_by_key(|image| image.id);
            for image in placed {
                for line in 0..image.height {
                    let from = line as usize * image.width as usize * BYTES;
                    let to = ((image.y + line) as usize * self.width as usize + image.x as usize)
                        * BYTES;
                    let length = image.width as usize * BYTES;
                    screen[to..to + length].copy_from_slice(&image.pixels[from..from + length]);
                }
            }
            screen
        }

        fn placed_ids(&self) -> Vec<u32> {
            self.placed.iter().map(|image| image.id).collect()
        }
    }

    fn field<'a>(control: &'a str, key: &str) -> Option<&'a str> {
        control
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    fn decode(payload: &str, compressed: bool) -> Vec<u8> {
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

    /// A frame holding `pixels`, so a test can change it the way the compositor
    /// would.
    fn frame_of(width: u32, height: u32, pixels: &[u8]) -> Frame {
        let wide: Vec<u8> = pixels
            .as_chunks::<BYTES>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 0xff])
            .collect();
        let mut frame = Frame::new(width, height);
        frame.draw(
            &Image {
                pixels: &wide,
                stride: width as usize * 4,
                width,
                height,
                format: SourceFormat::Xrgb8888,
            },
            Rect::new(0, 0, width, height),
            Rect::new(0, 0, width, height),
        );
        frame
    }

    /// A frame with one rectangle of it painted another colour.
    fn paint(frame: &mut Frame, rect: Rect, color: [u8; BYTES]) {
        let pixels: Vec<u8> = (0..rect.width * rect.height)
            .flat_map(|_| [color[2], color[1], color[0], 0xff])
            .collect();
        let image = Image {
            pixels: &pixels,
            stride: rect.width as usize * 4,
            width: rect.width,
            height: rect.height,
            format: SourceFormat::Xrgb8888,
        };
        frame.draw(&image, Rect::new(0, 0, rect.width, rect.height), rect);
    }

    fn capabilities() -> Capabilities {
        capabilities_of((80, 64))
    }

    fn capabilities_of(pixels: (u32, u32)) -> Capabilities {
        Capabilities {
            cell: (8, 8),
            cells: (pixels.0 / 8, pixels.1 / 8),
            pixels,
            terminal: None,
            graphics: true,
            keyboard: true,
            pixel_mouse: false,
            shared_memory: false,
            patches: true,
        }
    }

    /// A pane under test: the presenter, the terminal end of its socket, and
    /// what that terminal would be showing.
    struct Pane {
        presenter: Presenter,
        freed: calloop::channel::Channel<Event>,
        reader: std::io::BufReader<UnixStream>,
        terminal: Terminal,
    }

    impl Pane {
        fn new(capabilities: &Capabilities) -> Self {
            let (pane, terminal) = UnixStream::pair().expect("a socket pair");
            let (events, freed) = calloop::channel::channel();
            let presenter = Presenter::new(events).expect("a presenter");
            presenter.attach(pane, capabilities);
            Self {
                presenter,
                freed,
                reader: std::io::BufReader::new(terminal),
                terminal: Terminal::new(
                    capabilities.pixels.0,
                    capabilities.pixels.1,
                    capabilities.cell,
                ),
            }
        }

        /// Present a frame, and hand the terminal whatever came of it.
        ///
        /// A frame the terminal already shows is not written at all; the frame
        /// comes back either way.
        fn present(&mut self, frame: Frame) -> Option<String> {
            let pixels = frame.pixels().to_vec();
            self.presenter.present(frame);
            // The presenter writes a frame before it can see the ack for it, so
            // anything it wrote is in the socket by the time it is back.
            self.presenter.drawn();
            let back = self.wait();
            assert_eq!(back.pixels(), pixels, "the frame is the one presented");
            self.written()
        }

        /// The next frame the presenter gives back.
        fn wait(&self) -> Frame {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.freed.try_recv() {
                    Ok(Event::Free { frame }) => return frame,
                    Ok(Event::Failed(error)) => panic!("the presenter failed: {error}"),
                    Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("no frame came back: {error:?}"),
                }
            }
        }

        /// Whatever the presenter has written, giving the terminal all of it
        /// and returning the frame among it, if there was one.
        fn written(&mut self) -> Option<String> {
            let mut frame = None;
            while let Some(message) = self.message() {
                match message {
                    ToClient::Frame(escapes) => {
                        self.terminal.write(&escapes);
                        frame = Some(String::from_utf8_lossy(&escapes).into_owned());
                    }
                    // Escapes that are not a frame: a wipe, a title.
                    ToClient::Bytes(escapes) => self.terminal.write(&escapes),
                    other => panic!("a pane is not sent {other:?}"),
                }
            }
            frame
        }

        /// The next message the presenter has written, if any.
        fn message(&mut self) -> Option<ToClient> {
            let mut fds = [PollFd::new(self.reader.get_ref(), PollFlags::IN)];
            let ready = rustix::event::poll(&mut fds, Some(&Timespec::default()));
            if !ready.is_ok_and(|ready| ready > 0) && self.reader.buffer().is_empty() {
                return None;
            }
            Some(pane::read::<_, ToClient>(&mut self.reader).expect("a message"))
        }

        /// The pixels the terminal shows, and the images it holds.
        fn shown(&self) -> Vec<u8> {
            self.terminal.shown()
        }
    }

    #[test]
    fn what_a_presenter_writes_is_what_a_terminal_takes() {
        // The whole of a pane's way out: a frame goes to the socket as the
        // bytes a terminal takes, one image holding all of it, and comes back
        // when the terminal has it.
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        let pixels = frame.pixels().to_vec();

        let escapes = pane.present(frame).expect("a frame is written");
        assert!(
            escapes.starts_with("\x1b[?2026h"),
            "one synchronized update"
        );
        assert!(escapes.ends_with("\x1b[?2026l"));
        assert_eq!(
            escapes.matches("a=T,f=24").count(),
            1,
            "the screen is one image"
        );
        assert!(escapes.contains(",s=80,v=64,"), "at the frame's own size");
        assert!(!escapes.contains(",c=") && !escapes.contains(",r="));
        assert_eq!(pane.shown(), pixels);
    }

    #[test]
    fn a_change_that_starts_between_cells_lands_where_it_belongs() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        pane.present(frame);

        // A hover highlight: a few rows that begin between two cells.
        let mut next = frame_of(80, 64, &pane.shown());
        paint(&mut next, Rect::new(24, 13, 16, 3), [200, 30, 30]);
        let pixels = next.pixels().to_vec();
        pane.present(next);
        assert_eq!(pane.shown(), pixels, "the highlight is where it was drawn");
    }

    #[test]
    fn a_frame_the_terminal_already_shows_costs_nothing() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        let pixels = frame.pixels().to_vec();
        assert!(pane.present(frame).is_some());

        let mut same = Frame::new(80, 64);
        same.clear([9, 9, 9]);
        assert!(pane.present(same).is_none(), "nothing is written for it");
        assert_eq!(pane.shown(), pixels, "and the screen does not move");
    }

    #[test]
    fn a_frame_changed_in_one_place_goes_as_a_tile() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        assert!(pane.present(frame).is_some(), "the first frame is whole");

        let mut changed = frame_of(80, 64, &pane.shown());
        // A few pixels in the middle of the screen: one tile holds them.
        paint(&mut changed, Rect::new(32, 24, 8, 8), [200, 30, 30]);
        let pixels = changed.pixels().to_vec();

        let escapes = pane.present(changed).expect("a tile is written");
        assert_eq!(escapes.matches("a=T,f=24").count(), 1);
        assert!(
            escapes.contains(",s=64,v=16,"),
            "the tile is whole cells: {escapes}"
        );
        assert!(
            escapes.contains("\x1b[3;1H"),
            "placed in the cell it starts in: {escapes}"
        );
        assert_eq!(pane.shown(), pixels, "and the screen is the frame");
        assert!(
            pane.terminal
                .placed_ids()
                .iter()
                .any(|id| *id >= FIRST_PATCH_ID),
            "the tile is its own image"
        );
    }

    #[test]
    fn the_terminal_ends_up_with_the_frame_however_it_changes() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        pane.present(frame);

        let mut whole = 0;
        let mut patches = 0;
        for step in 0..40u8 {
            let mut next = frame_of(80, 64, &pane.shown());
            match step % 8 {
                // A corner changes.
                0 | 4 => paint(&mut next, Rect::new(0, 0, 8, 8), [step, 1, 2]),
                // A line is typed: several cells wide, and not whole ones.
                1 => paint(&mut next, Rect::new(16, 29, 32, 5), [3, 4, step]),
                // A block is drawn over the middle of the screen.
                2 => paint(&mut next, Rect::new(24, 16, 24, 24), [5, step, 6]),
                // The whole screen is another scene: this one goes whole.
                3 => paint(&mut next, Rect::new(0, 0, 80, 64), [step, 7, 8]),
                // Two tiles at once, one of them in the last column.
                5 => {
                    paint(&mut next, Rect::new(0, 0, 8, 8), [step, 9, 10]);
                    paint(&mut next, Rect::new(72, 56, 8, 8), [11, step, 12]);
                }
                // The pointer moves along a row, in steps of one pixel.
                6 => paint(
                    &mut next,
                    Rect::new(9 + (u32::from(step) % 7) as i32, 50, 3, 3),
                    [13, 14, step],
                ),
                // Nothing changes at all.
                _ => {}
            }
            let pixels = next.pixels().to_vec();
            match pane.present(next) {
                Some(escapes) if escapes.contains(",i=1,") => whole += 1,
                Some(_) => patches += 1,
                None => {}
            }
            assert_eq!(
                pane.shown(),
                pixels,
                "the screen is the frame after step {step}"
            );
        }
        assert!(
            patches > 20,
            "most steps are a patch, not a screen: {patches}"
        );
        assert!(whole > 0, "a screen's worth of change still goes whole");
    }

    #[test]
    fn the_tiles_the_terminal_holds_do_not_pile_up() {
        // A screen with room for more tiles than the terminal may hold.
        let (width, height) = (640, 256);
        let mut pane = Pane::new(&capabilities_of((width, height)));
        let mut frame = Frame::new(width, height);
        frame.clear([9, 9, 9]);
        pane.present(frame);

        // One tile at a time, each somewhere the frame has not changed before.
        let grid = Grid::of(&Frame::new(width, height), capabilities().cell);
        let mut resets = 0;
        for step in 0..(MAXIMUM_LIVE as u32 + 8) {
            let rect = grid.rect(step);
            let mut next = frame_of(width, height, &pane.shown());
            paint(&mut next, rect, [step as u8, 3, 4]);
            let pixels = next.pixels().to_vec();
            if pane
                .present(next)
                .is_some_and(|escapes| escapes.contains(",i=1,"))
            {
                resets += 1;
            }
            assert_eq!(pane.shown(), pixels, "the screen after step {step}");
            assert!(
                pane.terminal.placed_ids().len() <= MAXIMUM_LIVE + 1,
                "the terminal holds {} images after step {step}",
                pane.terminal.placed_ids().len()
            );
        }
        assert!(resets > 0, "the tiles were never cleared");
    }

    #[test]
    fn a_terminal_that_did_not_report_its_cells_gets_whole_frames() {
        let mut pane = Pane::new(&Capabilities {
            patches: false,
            ..capabilities()
        });
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        assert!(pane.present(frame).unwrap().contains(",i=1,"));

        let mut changed = frame_of(80, 64, &pane.shown());
        paint(&mut changed, Rect::new(32, 24, 8, 8), [200, 30, 30]);
        let pixels = changed.pixels().to_vec();
        assert!(
            pane.present(changed).unwrap().contains(",i=1,"),
            "a change goes as a whole frame"
        );
        assert_eq!(pane.shown(), pixels);

        let same = frame_of(80, 64, &pixels);
        assert!(
            pane.present(same).is_none(),
            "but a frame it already shows still costs nothing"
        );
    }

    #[test]
    fn a_wiped_screen_takes_a_whole_frame() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        pane.present(frame);
        let mut small = frame_of(80, 64, &pane.shown());
        paint(&mut small, Rect::new(32, 24, 8, 8), [200, 30, 30]);
        assert!(!pane.present(small).unwrap().contains(",i=1,"));

        // The terminal is wiped for a resize: every image goes with it.
        pane.presenter.clear();
        let mut next = frame_of(80, 64, &pane.shown());
        paint(&mut next, Rect::new(32, 24, 8, 8), [201, 30, 30]);
        let pixels = next.pixels().to_vec();
        assert!(
            pane.present(next).unwrap().contains(",i=1,"),
            "the frame after a wipe is whole"
        );
        assert_eq!(pane.shown(), pixels);
    }

    #[test]
    fn a_frame_the_terminal_is_behind_is_not_given_back_early() {
        let mut pane = Pane::new(&capabilities());
        let mut frame = Frame::new(80, 64);
        frame.clear([9, 9, 9]);
        pane.presenter.present(frame);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && pane.written().is_none() {
            std::thread::yield_now();
        }
        assert!(
            pane.freed.try_recv().is_err(),
            "the frame is the terminal's until it is drawn"
        );
        pane.presenter.drawn();
        assert_eq!(pane.wait().width, 80);
    }
}
