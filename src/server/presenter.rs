//! Per-pane frame encoding and output thread.
//!
//! The compositor draws a pane's whole screen and sends it with the tiles that
//! changed; this thread cuts those tiles out, encodes them as kitty graphics,
//! and writes them to the pane's socket. It runs on a thread of its own, so a
//! slow terminal holds up only its own pane.

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
    protocol::pane::{self, Capabilities},
    render::{BYTES, Frame, Tile},
};

/// The cell size and shared-memory support of the terminal a pane is in.
#[derive(Debug, Clone, Copy)]
struct Config {
    cell: (u32, u32),
    shared_memory: bool,
}

impl Default for Config {
    fn default() -> Self {
        // Until a pane says otherwise: a cell size that is wrong only distorts
        // pixels, and no shared memory, which is what an unknown terminal
        // takes.
        Self {
            cell: (10, 20),
            shared_memory: false,
        }
    }
}

/// One frame on its way to the terminal, with the tiles it was diffed against.
#[derive(Debug)]
struct InFlight {
    frame: Frame,
    tiles: Vec<Tile>,
}

#[derive(Debug)]
enum Message {
    Attach(Option<UnixStream>),
    Configure(Config),
    /// Let go of the pane, and say why.
    Detached(String),
    Frame {
        frame: Frame,
        tiles: Vec<Tile>,
    },
    Drawn,
    Raw(Vec<u8>),
}

/// What the presenter has to say to the server.
#[derive(Debug)]
pub enum Event {
    /// A frame the terminal has taken, and its tile list, for the compositor to
    /// draw the next one into.
    Free {
        frame: Frame,
        tiles: Vec<Tile>,
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

    /// The terminal's cell size or shared-memory support changed.
    pub fn configure(&self, capabilities: &Capabilities) {
        self.send(Message::Configure(Config {
            cell: capabilities.cell,
            shared_memory: capabilities.shared_memory,
        }));
    }

    pub fn detach(&self, reason: &str) {
        self.send(Message::Detached(reason.to_owned()));
        self.send(Message::Attach(None));
    }

    pub fn clear(&self) {
        self.raw(kitty::clear());
    }

    /// Put the tiles of a frame on the terminal.
    pub fn present(&self, frame: Frame, tiles: Vec<Tile>) {
        self.send(Message::Frame { frame, tiles });
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
    let mut pixels = Vec::new();
    let mut stats = Stats::default();
    let mut config = Config::default();
    let mut terminal: Option<UnixStream> = None;
    let mut in_flight: Option<InFlight> = None;
    while let Ok(message) = queue.recv() {
        let (frame, tiles) = match message {
            Message::Attach(attached) => {
                terminal = attached;
                if terminal.is_none() {
                    free(&mut in_flight, events)?;
                }
                continue;
            }
            Message::Configure(configuration) => {
                config = configuration;
                encoder.shared_memory = config.shared_memory;
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
            Message::Frame { frame, tiles } => (frame, tiles),
        };
        let count = tiles.len();

        // The tiles are cut out in the order they are listed: the compositor
        // lists them in grid order and the id each one keeps is the terminal's.
        let phase = Instant::now();
        out.clear();
        Encoder::begin_frame(&mut out);
        for tile in &tiles {
            let placement = placement(*tile, config.cell);
            cut(&frame, *tile, &mut pixels);
            encoder.transmit_and_place(&mut out, &pixels, placement);
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
        if terminal.is_none() {
            if events.send(Event::Free { frame, tiles }).is_err() {
                return Ok(());
            }
            continue;
        }

        stats.record(count, out.len(), spent_encoding, written);
        in_flight = Some(InFlight { frame, tiles });
    }
    Ok(())
}

/// Hand a frame the terminal has taken back to the compositor.
fn free(in_flight: &mut Option<InFlight>, events: &EventSender<Event>) -> std::io::Result<()> {
    let Some(InFlight { frame, tiles }) = in_flight.take() else {
        return Ok(());
    };
    events
        .send(Event::Free { frame, tiles })
        .map_err(|_| std::io::Error::other("the event loop is gone"))
}

/// Copy one tile out of the frame, rows end to end.
///
/// The frame's rows are its whole width apart; a tile's are not.
fn cut(frame: &Frame, tile: Tile, out: &mut Vec<u8>) {
    let rect = tile.rect;
    let stride = frame.width as usize * BYTES;
    let length = rect.width as usize * BYTES;
    out.clear();
    out.reserve(length * rect.height as usize);
    for row in 0..rect.height {
        let start = (rect.y as usize + row as usize) * stride + rect.x as usize * BYTES;
        out.extend_from_slice(&frame.pixels()[start..start + length]);
    }
}

/// Where one tile goes on a pane's screen, and what it is called.
///
/// The name is the tile's, and a tile keeps its name across frames, so the same
/// tile of the next frame replaces the image the terminal has.
fn placement(tile: Tile, cell: (u32, u32)) -> Placement {
    let (cell_width, cell_height) = (cell.0.max(1), cell.1.max(1));
    let rect = tile.rect;
    let cell_aligned =
        rect.width.is_multiple_of(cell_width) && rect.height.is_multiple_of(cell_height);
    let (cols, rows) = if cell_aligned {
        (rect.width / cell_width, rect.height / cell_height)
    } else {
        (0, 0)
    };
    Placement {
        id: tile.image,
        width: rect.width,
        height: rect.height,
        // c/r would scale a partial edge tile to a whole cell rectangle.
        // Zero leaves it at its native pixel size instead.
        cols,
        rows,
        cell: (rect.x as u32 / cell_width, rect.y as u32 / cell_height),
    }
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

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::{
        kitty::ImageId,
        protocol::pane::{self, ToClient},
        render::{Image, Rect, SourceFormat},
    };

    #[test]
    fn partial_edge_tiles_keep_native_dimensions() {
        let edge = placement(tile(Rect::new(0, 0, 7, 9), 1), (10, 20));
        assert_eq!((edge.cols, edge.rows), (0, 0));

        let full = placement(tile(Rect::new(0, 0, 160, 160), 2), (10, 20));
        assert_eq!((full.cols, full.rows), (16, 8));
    }

    #[test]
    fn a_tile_is_cut_out_of_the_frame_row_by_row() {
        // A four by two frame whose pixels count 0..8, and the middle two of
        // each row taken out: the rows of the tile end to end, not the frame's.
        let bytes: Vec<u8> = (0u8..8)
            .flat_map(|value| [value, value, value, 0xff])
            .collect();
        let mut frame = Frame::new(4, 2);
        frame.draw(
            &Image {
                pixels: &bytes,
                stride: 4 * 4,
                width: 4,
                height: 2,
                format: SourceFormat::Xrgb8888,
            },
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 4, 2),
        );

        let mut cut_out = Vec::new();
        cut(&frame, tile(Rect::new(1, 0, 2, 2), 1), &mut cut_out);
        assert_eq!(
            cut_out,
            vec![1, 1, 1, 2, 2, 2, 5, 5, 5, 6, 6, 6],
            "two pixels of the first row, then two of the second"
        );
    }

    fn tile(rect: Rect, image: u32) -> Tile {
        Tile {
            image: ImageId::new(image),
            rect,
        }
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
        // The whole of a pane's way out: a frame goes to the socket as the
        // bytes the terminal takes, and comes back when the pane has them.
        let (pane, terminal) = UnixStream::pair().expect("a socket pair");
        let (events, freed) = calloop::channel::channel();
        let presenter = Presenter::new(events).expect("a presenter");
        presenter.attach(pane, &capabilities());

        let mut frame = Frame::new(160, 160);
        frame.clear([9, 9, 9]);
        let tiles = vec![tile(Rect::new(0, 0, 160, 160), 1)];
        presenter.present(frame, tiles);

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
        assert!(escapes.windows(b"a=T,f=24".len()).any(|w| w == b"a=T,f=24"));

        // The pane has written it, so the frame is the compositor's again.
        presenter.drawn();
        let free = freed.recv().expect("the frame comes back");
        let Event::Free { frame, tiles } = free else {
            panic!("{free:?}");
        };
        assert_eq!(frame.width, 160);
        assert_eq!(tiles.len(), 1);
    }
}
