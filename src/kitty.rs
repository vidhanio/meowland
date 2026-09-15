//! Encoding of the [kitty graphics protocol][spec].
//!
//! Everything the compositor needs to put pixels on the terminal screen:
//! transmitting raw RGBA image data (zlib compressed) and placing it into a
//! cell rectangle in one command, deleting images again, and the handful of
//! terminal modes the protocol relies on.
//!
//! This module only ever *writes* escapes. Reading the terminal's answers lives
//! in [`crate::tty`], which is the only place allowed to touch stdin.
//!
//! [spec]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::io::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use flate2::{Compression, write::ZlibEncoder};
use smithay::input::pointer::CursorIcon;

/// Maximum size of a base64 plot line, mandated by the protocol.
const CHUNK: usize = 4096;

/// Z-index of the composited screen. Positive values draw above the terminal
/// text, so window content covers whatever the terminal last wrote in those
/// cells.
const Z_ABOVE_TEXT: i32 = 1;
/// Where a shared memory object lives, and what it is called.
const SHM_DIRECTORY: &str = "/dev/shm";

/// Remove every object this run put in shared memory that nobody took.
///
/// The terminal takes one per transfer and unlinks it, so anything left is from
/// a terminal that stopped reading - and an object is the size of the pixels it
/// holds. The names carry this process's id, so nothing belonging to another
/// compositor is at risk.
pub fn discard_shared_memory() {
    let prefix = format!("meowland-{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(SHM_DIRECTORY) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix)
            && let Err(err) = std::fs::remove_file(entry.path())
        {
            tracing::debug!(?err, "could not remove a shared memory object");
        }
    }
}

/// The id the startup probe uses for the tile it sends out of shared memory, so
/// that its answer can be told from the one the graphics query gives.
pub const SHARED_PROBE_ID: u32 = 78;

/// Send a one-pixel tile out of shared memory, to find out whether the terminal
/// can read one there.
///
/// Returns whether the tile was written at all; what the terminal makes of it
/// comes back on stdin with the rest of the probe's answers, and
/// [`discard_shared_probe`] cleans up after a terminal that did not read it.
pub fn shared_memory_probe(out: &mut Vec<u8>) -> bool {
    let Some(object) = Shared::write(0, &[0, 0, 0, 255]).ok() else {
        return false;
    };
    out.extend_from_slice(b"\x1b_G");
    let _ = write!(out, "a=q,f=32,t=s,i={SHARED_PROBE_ID},s=1,v=1;");
    out.extend_from_slice(&object.encoded_name());
    out.extend_from_slice(b"\x1b\\");
    // Kept in the module so the cleanup below can find it by name.
    *PROBE_OBJECT.lock().expect("no poison") = Some(object.name);
    true
}

/// Remove the probe's object, for a terminal that turned out not to read it.
pub fn discard_shared_probe() {
    let taken = PROBE_OBJECT.lock().expect("no poison").take();
    if let Some(name) = taken {
        Shared { name }.unlink();
    }
}

/// The probe object's name, until its fate is known.
static PROBE_OBJECT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// One tile's payload, in a shared memory object the terminal reads for itself.
///
/// The escape that carries it holds only the object's name, so the pixels never
/// travel through the pty at all - which is the difference between a few
/// kilobytes a frame and a few megabytes. The terminal unlinks the object once
/// it has read it, so this lives exactly as long as the transfer does.
#[derive(Debug)]
struct Shared {
    name: String,
}

impl Shared {
    /// Write `payload` into a new object, named after `sequence`.
    fn write(sequence: u64, payload: &[u8]) -> std::io::Result<Self> {
        use std::io::Write as _;

        // A POSIX shared memory name, which has to begin with a slash: kitty
        // refuses anything else outright ("POSIX SHM names must start with /"),
        // even though `shm_open` itself would accept it.
        let name = format!("/meowland-{}-{sequence}", std::process::id());
        let path = format!("{SHM_DIRECTORY}{name}");
        let file = rustix::fs::open(
            path.as_str(),
            rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL | rustix::fs::OFlags::RDWR,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        rustix::fs::ftruncate(&file, payload.len() as u64).map_err(std::io::Error::from)?;
        let mut file = std::fs::File::from(file);
        file.write_all(payload)?;
        file.flush()?;
        drop(file);
        Ok(Self { name })
    }

    /// The name as the escape carries it: base64, like any other payload.
    fn encoded_name(&self) -> Vec<u8> {
        let mut encoded = vec![0; base64::encoded_len(self.name.len(), true).expect("name fits")];
        let length = BASE64
            .encode_slice(self.name.as_bytes(), &mut encoded)
            .expect("the buffer has the exact encoded size");
        encoded.truncate(length);
        encoded
    }

    /// Remove it, for a terminal that turned out not to read it.
    fn unlink(self) {
        let path = format!("{SHM_DIRECTORY}{}", self.name);
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::debug!(?err, path, "could not remove a shared memory object");
        }
    }
}

/// The smallest saving that makes compressing a frame worth its time, as a
/// fraction of the pixels: below this the terminal is handed the pixels as they
/// are, because inflating costs it work and the bytes were never going to
/// shrink.
///
/// Compressing film-like content pays (it halves), and text pays enormously;
/// what does not pay is content that is already compressed or has no structure
/// to find, where zlib spends milliseconds to save a fraction of what base64
/// then adds straight back.
const WORTH_COMPRESSING: f64 = 0.75;

/// Reusable compression and base64 storage for tile transmissions.
#[derive(Debug)]
pub struct Encoder {
    zlib: ZlibEncoder<Vec<u8>>,
    payload: Vec<u8>,
    finished: bool,
    /// Whether the frame being encoded is compressed, decided once its first
    /// tile has shown what the content does. `None` means the frame has not
    /// shown anything yet.
    compress: Option<bool>,
    /// Whether the last frame was compressed, for the log.
    pub compressed_last_frame: bool,
    /// Whether the terminal reads tiles out of shared memory, which is what
    /// keeps the pixels off the pty.
    pub shared_memory: bool,
    /// Names shared memory objects, since a terminal takes one per transfer.
    transfers: u64,
}

impl Default for Encoder {
    fn default() -> Self {
        Self {
            zlib: ZlibEncoder::new(Vec::new(), Compression::fast()),
            payload: Vec::new(),
            finished: false,
            compress: None,
            compressed_last_frame: true,
            shared_memory: false,
            transfers: 0,
        }
    }
}

/// Image identity and terminal-cell placement for one transmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    /// The cell the tile's first pixel belongs in: the cursor has to be there
    /// before the image is placed, and whoever writes the escape has to know.
    pub cell: (u32, u32),
}

/// Set the terminal's mouse pointer shape, or reset it to the terminal's own
/// default.
///
/// This is the one part of the pointer the terminal draws better than we can:
/// its pointer is a real pointer, and clients usually say what they want shown
/// (a text beam, a resize arrow, a hand over a link) through
/// `wp_cursor_shape_manager_v1`.
pub fn set_pointer_shape(out: &mut Vec<u8>, shape: Option<&str>) {
    match shape {
        Some(shape) => {
            let _ = write!(out, "\x1b]22;{shape}\x1b\\");
        }
        None => out.extend_from_slice(b"\x1b]22;\x1b\\"),
    }
}

/// The [pointer shape name][names] that stands for a Wayland cursor icon.
///
/// [names]: https://sw.kovidgoyal.net/kitty/pointer-shapes/#pointer-shape-names
pub const fn pointer_shape(icon: CursorIcon) -> &'static str {
    match icon {
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize | CursorIcon::ColResize => "ew-resize",
        CursorIcon::NsResize | CursorIcon::RowResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        // The terminal's set of shapes has no name for these; the four-way arrow is the closest
        // thing it does have.
        CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::AllResize => "move",
        _ => "default",
    }
}

/// Move the cursor to a cell. Coordinates are 1-based, like the escape code it
/// turns into.
pub fn cursor_to(out: &mut Vec<u8>, col: u32, row: u32) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

/// Delete every image, freeing the terminal's memory for them.
pub fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

/// Transmit `pixels` as the image `id`, and place it in the cell rectangle that
/// starts at the cursor.
///
/// Doing both in one command is what makes an update cheap and flicker free: an
/// image id is replaced atomically (the old placement disappears with the old
/// data), so a tile can be re-sent while it is on screen. `C=1` keeps the
/// cursor where it is, so placing an image never scrolls the terminal and never
/// moves the anchor the next tile is addressed from.
impl Encoder {
    /// Start a frame: the terminal buffers everything until
    /// [`Encoder::end_frame`], which is what keeps a frame from tearing.
    ///
    /// Nothing of the encoder's changes here - the frame's own state is what
    /// the first tile will decide - so this is the frame, not the encoder.
    pub fn begin_frame(out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026h");
    }

    /// End a frame, with whatever the next one's first tile shows deciding
    /// whether that frame is compressed.
    pub fn end_frame(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026l");
        self.compress = None;
    }

    /// Compress, if it pays, and transmit `pixels` as image `id` at the cursor.
    pub fn transmit_and_place(&mut self, out: &mut Vec<u8>, pixels: &[u8], placement: Placement) {
        debug_assert_eq!(
            pixels.len(),
            placement.width as usize * placement.height as usize * 4
        );
        cursor_to(out, placement.cell.0, placement.cell.1);

        // The first tile of a frame decides for the rest of it: whether
        // compressing pays is a property of what the frame is *of*, and one
        // tile answers for all of them.
        let shared = self.shared_memory.then(|| {
            self.transfers += 1;
            self.transfers
        });
        // Whether compressing pays is a decision about the pty: with shared
        // memory the pixels never travel through it, so the answer is no - it
        // would be our time against the terminal's, and reading pixels costs
        // the terminal less than inflating them.
        if self.shared_memory {
            self.compressed_last_frame = false;
            self.compress = Some(false);
            transmit(out, &mut self.payload, pixels, placement, false, shared);
            return;
        }
        let Some(compress) = self.compress else {
            // Compressed before being asked whether to: one tile of work is
            // what it costs to find out.
            let compressed_len = self.compress(pixels);
            let worth = compressed_len < (pixels.len() as f64 * WORTH_COMPRESSING) as usize;
            self.compress = Some(worth);
            self.compressed_last_frame = worth;
            let payload = if worth { self.zlib.get_ref() } else { pixels };
            transmit(out, &mut self.payload, payload, placement, worth, shared);
            return;
        };

        if compress {
            self.compress(pixels);
            let payload = self.zlib.get_ref();
            transmit(out, &mut self.payload, payload, placement, true, shared);
        } else {
            transmit(out, &mut self.payload, pixels, placement, false, shared);
        }
    }

    /// Compress `pixels`, leaving the result in the encoder's buffer.
    fn compress(&mut self, pixels: &[u8]) -> usize {
        if self.finished {
            let mut compressed = self
                .zlib
                .reset(Vec::new())
                .expect("resetting a Vec encoder cannot fail");
            compressed.clear();
            *self.zlib.get_mut() = compressed;
            self.finished = false;
        }
        self.zlib.get_mut().reserve(pixels.len() / 8);
        // Compositor output is mostly flat color, so compressing it typically
        // shrinks a tile by an order of magnitude.
        self.zlib
            .write_all(pixels)
            .expect("writing to a Vec cannot fail");
        self.zlib.try_finish().expect("finishing a Vec cannot fail");
        self.finished = true;
        self.zlib.get_ref().len()
    }
}

/// Get `payload` to the terminal: in a shared memory object if it reads those,
/// otherwise base64'd through the pty.
fn transmit(
    out: &mut Vec<u8>,
    encoded: &mut Vec<u8>,
    payload: &[u8],
    placement: Placement,
    compressed: bool,
    shared: Option<u64>,
) {
    if let Some(sequence) = shared {
        let object = match Shared::write(sequence, payload) {
            Ok(object) => object,
            // Out of shared memory, or a name already taken: the pty still
            // works, so this is not worth failing a frame over.
            Err(err) => {
                tracing::debug!(?err, "could not put a tile in shared memory");
                return direct(out, encoded, payload, placement, compressed);
            }
        };
        placed(out, placement, compressed, "t=s", &object.encoded_name());
        return;
    }
    direct(out, encoded, payload, placement, compressed);
}

/// The payload base64'd into the escape, in chunks.
fn direct(
    out: &mut Vec<u8>,
    encoded: &mut Vec<u8>,
    payload: &[u8],
    placement: Placement,
    compressed: bool,
) {
    let encoded_len =
        base64::encoded_len(payload.len(), true).expect("a tile fits in address space");
    encoded.resize(encoded_len, 0);
    let payload_len = BASE64
        .encode_slice(payload, encoded)
        .expect("the payload buffer has the exact encoded size");
    chunked(out, placement, compressed, &encoded[..payload_len]);
}

fn chunked(out: &mut Vec<u8>, placement: Placement, compressed: bool, payload: &[u8]) {
    let mut chunks = payload.chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let head = if first {
            describe(placement, compressed, None)
        } else {
            String::new()
        };
        first = false;
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(if chunks.peek().is_some() {
            b"m=1;"
        } else {
            b"m=0;"
        });
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// One escape, whose payload is not pixels but where to find them.
fn placed(out: &mut Vec<u8>, placement: Placement, compressed: bool, medium: &str, payload: &[u8]) {
    out.extend_from_slice(b"\x1b_G");
    let head = describe(placement, compressed, Some(medium));
    out.extend_from_slice(head.as_bytes());
    // The control data ends where the payload begins, and the payload here is a
    // name rather than pixels.
    out.extend_from_slice(b";");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b\\");
}

/// The control data every transmission carries, whatever the medium.
fn describe(placement: Placement, compressed: bool, medium: Option<&str>) -> String {
    let Placement {
        id,
        width,
        height,
        cols,
        rows,
        ..
    } = placement;
    let compression = if compressed { "o=z," } else { "" };
    let medium = medium.map_or(String::new(), |medium| format!("{medium},"));
    format!(
        "a=T,f=32,{compression}{medium}s={width},v={height},i={id},p={id},c={cols},r={rows},C=1,z={Z_ABOVE_TEXT},q=2,"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoding of one escape sequence, following the protocol by the book:
    /// this is the independent side of the round trip, so a wrong chunk
    /// size, format or byte order fails.
    #[derive(Debug, PartialEq, Eq)]
    enum Command {
        Transmit {
            id: u32,
            width: u32,
            height: u32,
            compressed: bool,
            pixels: Vec<u8>,
        },
        Put {
            id: u32,
            placement: u32,
            cols: u32,
            rows: u32,
            moves_cursor: bool,
        },
        DeleteAll,
    }

    fn decode(stream: &[u8]) -> Vec<Command> {
        let text = std::str::from_utf8(stream).expect("escapes are ASCII");
        let mut commands = Vec::new();
        let mut rest = text;
        // Partial transmissions are reassembled before being interpreted: the
        // header comes from the first chunk, the payload from all of
        // them.
        let mut pending: Option<(String, String)> = None;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").expect("unterminated escape");
            let (control, payload) = body[..end].split_once(';').unwrap_or((&body[..end], ""));
            assert!(payload.len() <= CHUNK, "chunk exceeds the protocol limit");
            rest = &body[end + 2..];

            let (header, data) = match pending.take() {
                Some((header, mut data)) => {
                    data.push_str(payload);
                    (header, data)
                }
                None => (control.to_owned(), payload.to_owned()),
            };
            if control.contains("m=1") {
                pending = Some((header, data));
            } else {
                commands.extend(interpret(&header, &data));
            }
        }
        assert!(pending.is_none(), "stream ends mid-transmission");
        commands
    }

    fn field<'a>(header: &'a str, key: &str) -> Option<&'a str> {
        header
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    fn number(header: &str, key: &str, default: u32) -> u32 {
        field(header, key).map_or(default, |v| v.parse().unwrap())
    }

    /// Interpret one reassembled graphics command. A command can transmit *and*
    /// place, so this returns a list.
    fn interpret(header: &str, payload: &str) -> Vec<Command> {
        let action = field(header, "a");
        let mut commands = Vec::new();
        if matches!(action, Some("t" | "T")) {
            assert_eq!(field(header, "f"), Some("32"), "we only send RGBA");
            let compressed = field(header, "o") == Some("z");
            let (width, height) = (number(header, "s", 0), number(header, "v", 0));
            let raw = BASE64
                .decode(payload)
                .expect("payload must be valid base64");
            let pixels = if compressed {
                zlib_decompress(&raw)
            } else {
                raw
            };
            assert_eq!(
                pixels.len(),
                width as usize * height as usize * 4,
                "pixel data must match the declared geometry"
            );
            commands.push(Command::Transmit {
                id: number(header, "i", 0),
                width,
                height,
                compressed,
                pixels,
            });
        }
        if matches!(action, Some("p" | "T")) {
            commands.push(Command::Put {
                id: number(header, "i", 0),
                placement: number(header, "p", 0),
                cols: number(header, "c", 0),
                rows: number(header, "r", 0),
                moves_cursor: field(header, "C") != Some("1"),
            });
        }
        if action == Some("d") {
            commands.push(Command::DeleteAll);
        }
        assert!(!commands.is_empty(), "unexpected action {action:?}");
        commands
    }

    fn zlib_decompress(data: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut out)
            .expect("payload must be a zlib stream");
        out
    }

    fn test_pixels(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
            }
        }
        pixels
    }

    #[test]
    fn a_tile_round_trips_pixels_exactly() {
        // Large enough to need several chunks, so chunk reassembly is covered
        // too.
        let (width, height) = (64, 48);
        let pixels = test_pixels(width, height);
        let mut out = Vec::new();
        Encoder::default().transmit_and_place(
            &mut out,
            &pixels,
            Placement {
                id: 7,
                width,
                height,
                cols: 6,
                rows: 4,
                cell: (0, 0),
            },
        );

        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id: 7,
                    width,
                    height,
                    compressed: true,
                    pixels,
                },
                Command::Put {
                    id: 7,
                    placement: 7,
                    cols: 6,
                    rows: 4,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn an_encoder_can_be_reused() {
        // Flat content, so this is about reusing the encoder and nothing else:
        // what it decides about compressing is the subject of its own test.
        let flat: Vec<u8> = [1u8, 2, 3, 255].repeat(16);
        let mut encoder = Encoder::default();
        let mut out = Vec::new();
        encoder.transmit_and_place(
            &mut out,
            &flat,
            Placement {
                id: 1,
                width: 4,
                height: 4,
                cols: 1,
                rows: 1,
                cell: (0, 0),
            },
        );
        out.clear();
        let flat: Vec<u8> = [4u8, 5, 6, 255].repeat(16);
        encoder.transmit_and_place(
            &mut out,
            &flat,
            Placement {
                id: 2,
                width: 4,
                height: 4,
                cols: 1,
                rows: 1,
                cell: (0, 0),
            },
        );

        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id: 2,
                    width: 4,
                    height: 4,
                    compressed: true,
                    pixels: [4u8, 5, 6, 255].repeat(16),
                },
                Command::Put {
                    id: 2,
                    placement: 2,
                    cols: 1,
                    rows: 1,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn every_tile_of_a_compressed_frame_carries_its_own_pixels() {
        // A frame is decided once, but every tile of it is still its own
        // picture: reusing the decision must not reuse the bytes.
        let flat: Vec<u8> = [7u8, 8, 9, 255].repeat(16);
        let other: Vec<u8> = [10u8, 11, 12, 255].repeat(16);

        let mut encoder = Encoder::default();
        let mut out = Vec::new();
        for (index, pixels) in [&flat, &other].iter().enumerate() {
            encoder.transmit_and_place(
                &mut out,
                pixels,
                Placement {
                    id: index as u32,
                    width: 4,
                    height: 4,
                    cols: 1,
                    rows: 1,
                    cell: (0, 0),
                },
            );
        }

        let transmitted: Vec<Vec<u8>> = decode(&out)
            .into_iter()
            .filter_map(|command| match command {
                Command::Transmit { pixels, .. } => Some(pixels),
                _ => None,
            })
            .collect();
        assert_eq!(transmitted, vec![flat, other]);
    }

    #[test]
    fn a_shared_memory_transfer_names_the_object_in_its_payload() {
        let name = "/meowland-0-1";
        let object = Shared {
            name: name.to_owned(),
        };
        let mut out = Vec::new();
        placed(
            &mut out,
            Placement {
                id: 3,
                width: 160,
                height: 160,
                cols: 16,
                rows: 8,
                cell: (0, 0),
            },
            true,
            "t=s",
            &object.encoded_name(),
        );

        // The name is the payload, so it has to come after the separator and
        // nothing else - a terminal reading `q=2,name` as control data would
        // never load the image.
        let text = std::str::from_utf8(&out).expect("escapes are ascii");
        let (head, payload) = text
            .trim_start_matches("\x1b_G")
            .trim_end_matches("\x1b\\")
            .split_once(';')
            .expect("the escape separates control data from its payload");
        assert!(head.contains("t=s"), "{head}");
        // A POSIX shared memory name begins with a slash, and a terminal that
        // checks will refuse one that does not.
        assert!(name.starts_with('/'), "{name}");
        // The name travels base64'd, like every other payload in this protocol.
        assert_eq!(
            payload,
            String::from_utf8(object.encoded_name()).expect("base64 is ascii")
        );
    }

    #[test]
    fn content_that_does_not_shrink_is_sent_as_it_is() {
        // Nothing for zlib to find: compressing would spend milliseconds to
        // save a fraction of what base64 then adds back, so the pixels go as
        // they are and the decoder has only base64 to undo.
        let mut state = 0x1234_5678u32;
        let noise: Vec<u8> = (0..160 * 160 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();

        let mut encoder = Encoder::default();
        let mut out = Vec::new();
        encoder.transmit_and_place(
            &mut out,
            &noise,
            Placement {
                id: 1,
                width: 160,
                height: 160,
                cols: 16,
                rows: 8,
                cell: (0, 0),
            },
        );

        let commands = decode(&out);
        assert_eq!(
            commands,
            vec![
                Command::Transmit {
                    id: 1,
                    width: 160,
                    height: 160,
                    compressed: false,
                    pixels: noise,
                },
                Command::Put {
                    id: 1,
                    placement: 1,
                    cols: 16,
                    rows: 8,
                    moves_cursor: false,
                },
            ]
        );
        assert!(!encoder.compressed_last_frame);
    }

    /// Full-frame cost of each kind of content, for the record in the log.
    #[test]
    #[ignore = "measurement: run with --ignored --nocapture"]
    fn cost_of_a_frame_by_content() {
        use std::time::Instant;

        let content = |kind: &str| -> Vec<u8> {
            let mut state = 0x9e37_79b9u32;
            (0..160 * 160 * 4)
                .map(|i| match kind {
                    "text" => {
                        if (i / 4 / 3 + i / 4 / 160 / 7) % 5 == 0 {
                            0xd0
                        } else {
                            0x18
                        }
                    }
                    "video" => {
                        let pixel = i / 4;
                        let (x, y) = (pixel % 160, pixel / 160);
                        let a = ((x + y) / 4) % 64;
                        (0x30 + a) as u8
                    }
                    _ => {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        (state >> 24) as u8
                    }
                })
                .collect()
        };

        for kind in ["text", "video", "noise"] {
            // `None` on every tile is what the encoder always did: compress
            // everything. `Some(false)` is the other extreme, and `None` once
            // (the shipped policy) decides from the first tile.
            for forced in ["always", "as-is", "policy"] {
                let pixels = content(kind);
                let mut encoder = Encoder::default();
                let mut out = Vec::new();
                let mut bytes = 0;
                let before = Instant::now();
                for step in 0..30 {
                    out.clear();
                    // `always` is exactly what the encoder used to do: compress
                    // every tile, whatever the content.
                    if forced == "always" {
                        encoder.compress(&pixels);
                        let placement = Placement {
                            id: step % 28,
                            width: 160,
                            height: 160,
                            cols: 16,
                            rows: 8,
                            cell: (0, 0),
                        };
                        let payload = encoder.zlib.get_ref();
                        transmit(
                            &mut out,
                            &mut encoder.payload,
                            payload,
                            placement,
                            true,
                            None,
                        );
                        bytes = out.len();
                        continue;
                    }
                    encoder.compress = match forced {
                        "as-is" => Some(false),
                        _ => {
                            if step == 0 {
                                None
                            } else {
                                encoder.compress
                            }
                        }
                    };
                    encoder.transmit_and_place(
                        &mut out,
                        &pixels,
                        Placement {
                            id: step % 28,
                            width: 160,
                            height: 160,
                            cols: 16,
                            rows: 8,
                            cell: (0, 0),
                        },
                    );
                    bytes = out.len();
                }
                let ms = before.elapsed().as_secs_f64() * 1e3 / 30.0;
                let mode = format!("{forced:>7}");
                println!(
                    "{kind:>5} {mode}: {ms:>5.2} ms/tile  {:>6.1} KiB/tile  (a screen = {:>5.1} ms, {:>5.1} MiB)",
                    bytes as f64 / 1024.0,
                    ms * 28.0,
                    28.0 * bytes as f64 / (1024.0 * 1024.0),
                );
            }
        }
    }

    /// One image of the whole screen against the tiles that make it up, for the
    /// dense frame a film or a game produces.
    #[test]
    #[ignore = "measurement: run with --ignored --nocapture"]
    fn cost_of_one_image_against_many_tiles() {
        use std::time::Instant;

        const SCREEN: (u32, u32) = (1000, 600);
        const TILE: (u32, u32) = (160, 160);
        let grid = (SCREEN.0.div_ceil(TILE.0), SCREEN.1.div_ceil(TILE.1));
        let whole = film(SCREEN);
        let mut encoder = Encoder::default();
        let mut out = Vec::new();

        let before = Instant::now();
        for _ in 0..30 {
            out.clear();
            encoder.transmit_and_place(&mut out, &whole, whole_screen(SCREEN));
        }
        let one = before.elapsed().as_secs_f64() * 1e3 / 30.0;
        let one_bytes = out.len();

        let mut tile_pixels = Vec::new();
        let before = Instant::now();
        for _ in 0..30 {
            out.clear();
            for gy in 0..grid.1 {
                for gx in 0..grid.0 {
                    let placement = tile_placement(SCREEN, TILE, grid, (gx, gy));
                    cut_out(&whole, SCREEN, placement, &mut tile_pixels);
                    encoder.transmit_and_place(&mut out, &tile_pixels, placement);
                }
            }
        }
        let many = before.elapsed().as_secs_f64() * 1e3 / 30.0;
        let many_bytes = out.len();

        println!(
            "one image: {one:>5.2} ms, {one_bytes:>7} bytes    {} tiles: {many:>5.2} ms, {many_bytes:>7} bytes",
            grid.0 * grid.1,
        );
    }

    /// Film-like pixels: gradients with a little grain, which is what makes a
    /// frame expensive.
    fn film(size: (u32, u32)) -> Vec<u8> {
        let mut state = 0x5eed_1234u32;
        (0..size.0 * size.1 * 4)
            .map(|i| {
                let pixel = i / 4;
                let (x, y) = (pixel % size.0, pixel / size.0);
                if i % 4 == 3 {
                    255
                } else {
                    let base = ((x / 4 + y / 4) % 64) as u8;
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    0x20 + base + (state % 8) as u8
                }
            })
            .collect()
    }

    fn whole_screen(size: (u32, u32)) -> Placement {
        Placement {
            id: 0,
            width: size.0,
            height: size.1,
            cols: size.0.div_ceil(10),
            rows: size.1.div_ceil(20),
            cell: (0, 0),
        }
    }

    fn tile_placement(
        size: (u32, u32),
        tile: (u32, u32),
        grid: (u32, u32),
        at: (u32, u32),
    ) -> Placement {
        let (x, y) = (at.0 * tile.0, at.1 * tile.1);
        Placement {
            id: at.1 * grid.0 + at.0 + 1,
            width: tile.0.min(size.0 - x),
            height: tile.1.min(size.1 - y),
            cols: TILE_CELLS.0,
            rows: TILE_CELLS.1,
            cell: (x / 10, y / 20),
        }
    }

    /// The pixels of one tile, row by row, out of the frame it is part of.
    fn cut_out(frame: &[u8], size: (u32, u32), placement: Placement, into: &mut Vec<u8>) {
        into.clear();
        for row in 0..placement.height {
            let start =
                ((placement.cell.1 * 20 + row) * size.0 + placement.cell.0 * 10) as usize * 4;
            into.extend_from_slice(&frame[start..start + placement.width as usize * 4]);
        }
    }

    /// The grid a frame is divided into, in cells rather than pixels.
    const TILE_CELLS: (u32, u32) = (16, 8);

    #[test]
    fn every_chunk_is_a_whole_base64_line() {
        // Noise, so zlib cannot shrink the payload below one chunk.
        let (width, height) = (256, 256);
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        let mut state = 0x1234_5678u32;
        for _ in 0..width * height {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.extend_from_slice(&state.to_le_bytes());
        }
        let mut out = Vec::new();
        Encoder::default().transmit_and_place(
            &mut out,
            &pixels,
            Placement {
                id: 1,
                width,
                height,
                cols: 20,
                rows: 10,
                cell: (0, 0),
            },
        );

        let mut rest = std::str::from_utf8(&out).unwrap();
        let mut chunks = 0;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").unwrap();
            let (control, payload) = body[..end].split_once(';').unwrap();
            assert!(payload.len() <= CHUNK, "chunk longer than 4096 bytes");
            assert_eq!(payload.len() % 4, 0, "chunks must be whole base64 quanta");
            if control.contains("m=1") {
                // Anything followed by more data has to fill a whole chunk, or
                // the terminal would be handed a payload it
                // cannot decode when it concatenates.
                assert_eq!(
                    payload.len(),
                    CHUNK,
                    "a chunk with a successor must be full"
                );
            }
            chunks += 1;
            rest = &body[end + 2..];
        }
        assert!(chunks > 1, "this test needs a multi-chunk payload");
    }

    #[test]
    fn pointer_shapes_use_the_names_the_terminal_knows() {
        assert_eq!(pointer_shape(CursorIcon::Default), "default");
        assert_eq!(pointer_shape(CursorIcon::Text), "text");
        assert_eq!(pointer_shape(CursorIcon::Pointer), "pointer");
        // Icons the terminal set has no separate name for share the nearest
        // one.
        assert_eq!(pointer_shape(CursorIcon::ColResize), "ew-resize");
        assert_eq!(pointer_shape(CursorIcon::RowResize), "ns-resize");
        assert_eq!(pointer_shape(CursorIcon::AllScroll), "move");
        assert_eq!(pointer_shape(CursorIcon::ContextMenu), "default");
    }

    #[test]
    fn setting_a_pointer_shape_is_one_escape() {
        let mut out = Vec::new();
        set_pointer_shape(&mut out, Some("text"));
        set_pointer_shape(&mut out, None);
        assert_eq!(out, b"\x1b]22;text\x1b\\\x1b]22;\x1b\\");
    }

    #[test]
    fn deleting_everything_is_a_single_escape() {
        let mut out = Vec::new();
        delete_all(&mut out);
        assert_eq!(decode(&out), vec![Command::DeleteAll]);
    }

    #[test]
    fn tilde_the_cursor_is_addressed_one_based() {
        let mut out = Vec::new();
        cursor_to(&mut out, 0, 0);
        cursor_to(&mut out, 3, 11);
        assert_eq!(out, b"\x1b[1;1H\x1b[12;4H");
    }
}
