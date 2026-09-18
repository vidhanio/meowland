//! Kitty graphics protocol encoding, and the escapes that go with it.

use std::{
    io::Write as _,
    sync::atomic::{AtomicU32, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use flate2::{Compression, write::ZlibEncoder};
use nutype::nutype;
use rustix::shm::{self, Mode, OFlags};
use smithay::input::pointer::CursorIcon;

/// An image ID in the kitty graphics protocol.
///
/// A pane's frame keeps its ID, so the terminal replaces the image it already
/// has instead of drawing a second one.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct ImageId(u32);

/// Maximum size of a base64 plot line, mandated by the protocol.
const CHUNK: usize = 4096;

/// Z-index of the composited screen. A positive value draws above the terminal
/// text.
const Z_ABOVE_TEXT: i32 = 1;

/// Where Linux keeps the objects `shm_open` names.
///
/// Nothing lists them the way [`std::fs`] lists a directory, and a run that was
/// killed leaves its own behind, so this is the one place the filesystem is
/// read: everything else goes through [`rustix::shm`].
const SHM_DIRECTORY: &str = "/dev/shm";

/// Unlink every shared memory object this run left behind.
///
/// The terminal takes one object per transfer and unlinks it. Anything left
/// holds one frame's pixels and was never read. The names carry the process
/// id and the namespace, so no live object of another process is touched.
fn discard_shared_memory(namespace: u32) {
    let prefix = format!("/meowland-{}-{namespace}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(SHM_DIRECTORY) else {
        return;
    };
    for entry in entries.flatten() {
        let name = format!("/{}", entry.file_name().to_string_lossy());
        if name.starts_with(&prefix)
            && let Err(err) = shm::unlink(name.as_str())
        {
            tracing::debug!(?err, name, "could not unlink a shared memory object");
        }
    }
}

/// The namespace of the startup probe's object. No encoder uses it.
const PROBE_NAMESPACE: u32 = 0;

/// The namespace of the next encoder.
///
/// Each encoder has its own, so one pane's frames never overwrite an object
/// that another pane still reads.
static NEXT_NAMESPACE: AtomicU32 = AtomicU32::new(PROBE_NAMESPACE + 1);

/// The image id of the probe's graphics support query. Its answer is the one
/// that says whether the terminal speaks the protocol.
pub const GRAPHICS_PROBE_ID: ImageId = ImageId::new(77);

/// The image id of the probe's shared memory object, distinct from the graphics
/// query's id.
pub const SHARED_PROBE_ID: ImageId = ImageId::new(78);

/// The image a pane's frames are sent under.
///
/// A pane has one frame on its terminal at a time, so one id does: the terminal
/// replaces the image it holds as the next frame arrives.
const FRAME_ID: ImageId = ImageId::new(1);

/// The first image id a patch may take. Id 1 is the pane's screen.
pub const FIRST_PATCH_ID: u32 = 2;

/// One rectangle of a frame, sent as its own image and placed over the image
/// the terminal already has.
///
/// The rectangle's top-left pixel is the top-left pixel of `cell`, so a patch
/// is placed by the cursor alone: no cell rectangle scales it, and nothing
/// about it depends on where the pane is on the terminal's screen.
#[derive(Debug, Clone, Copy)]
pub struct Patch<'a> {
    pub id: ImageId,
    /// The cell the rectangle starts at.
    pub cell: (u32, u32),
    pub size: (u32, u32),
    pub pixels: &'a [u8],
}

/// Delete images, and the pixels the terminal holds for them.
///
/// A patch that another one has grown over, or that the screen has been wiped
/// with, is pixels the terminal would otherwise keep for nothing.
pub fn delete_images(out: &mut Vec<u8>, ids: &[ImageId]) {
    for id in ids {
        let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\");
    }
}

/// Send a one-pixel image out of shared memory, to test whether the terminal
/// reads one there. The guard removes the object if it did not.
pub fn shared_memory_probe(out: &mut Vec<u8>) -> Option<SharedProbe> {
    let object = Shared::new(PROBE_NAMESPACE, 0);
    if object.write(&[0, 0, 0, 255]).is_err() {
        return None;
    }
    out.extend_from_slice(b"\x1b_G");
    let _ = write!(out, "a=q,f=32,t=s,i={SHARED_PROBE_ID},s=1,v=1;");
    out.extend_from_slice(&object.encoded_name);
    out.extend_from_slice(b"\x1b\\");
    Some(SharedProbe(object))
}

#[derive(Debug)]
pub struct SharedProbe(Shared);

impl Drop for SharedProbe {
    fn drop(&mut self) {
        self.0.unlink();
    }
}

/// One frame's payload, in a shared memory object the terminal reads for
/// itself.
///
/// The escape carries the name only, so the pixels stay off the pty. The
/// terminal unlinks the object after reading it, and the next transfer makes it
/// again.
#[derive(Debug)]
struct Shared {
    /// The POSIX name, slash and all: what `shm_open` takes, and what the
    /// escape carries base64'd.
    name: String,
    encoded_name: Vec<u8>,
}

impl Shared {
    /// The object that holds one frame of an encoder's namespace.
    ///
    /// The name gives the process, the encoder and the slot, and an encoder
    /// keeps one slot: an object the terminal has not read yet is left alone,
    /// and that frame goes through the pty instead, which keeps both frames in
    /// order.
    fn new(namespace: u32, slot: u32) -> Self {
        let name = format!("/meowland-{}-{namespace}-{slot}", std::process::id());
        let mut encoded_name = vec![0; base64::encoded_len(name.len(), true).expect("name fits")];
        BASE64
            .encode_slice(name.as_bytes(), &mut encoded_name)
            .expect("the buffer has the exact encoded size");
        Self { name, encoded_name }
    }

    /// Write one frame into its object.
    ///
    /// An object the terminal still holds is never overwritten: creation fails,
    /// and that update goes through the pty instead, keeping both updates in
    /// order.
    fn write(&self, payload: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;

        let file = shm::open(
            self.name.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        rustix::fs::ftruncate(&file, payload.len() as u64).map_err(std::io::Error::from)?;
        let mut file = std::fs::File::from(file);
        file.write_all(payload)?;
        file.flush()?;
        Ok(())
    }

    fn unlink(&self) {
        match shm::unlink(self.name.as_str()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(err) => tracing::debug!(
                ?err,
                name = self.name,
                "could not unlink a shared memory object"
            ),
        }
    }
}

/// The smallest saving that makes compressing a frame worth the time, as a
/// fraction of the pixels. Below it the terminal gets the pixels as they are,
/// because inflating costs it work.
///
/// Film-like content halves when compressed and text shrinks far more. Content
/// already compressed does not shrink.
const COMPRESSION_RATIO: (usize, usize) = (3, 4);

#[derive(Debug)]
pub struct Encoder {
    zlib: ZlibEncoder<Vec<u8>>,
    payload: Vec<u8>,
    finished: bool,
    /// Whether the terminal reads frames out of shared memory, which keeps
    /// their pixels off the pty.
    pub shared_memory: bool,
    /// Which encoder this is. A pane's frames are named after it, so two panes
    /// never name the same object.
    namespace: u32,
    /// The object this encoder's frames go through, made once and reused.
    shared: Option<Shared>,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            zlib: ZlibEncoder::new(Vec::new(), Compression::fast()),
            payload: Vec::new(),
            finished: false,
            shared_memory: false,
            namespace: NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed),
            shared: None,
        }
    }
}

impl Drop for Encoder {
    /// Remove the objects the terminal did not take.
    fn drop(&mut self) {
        discard_shared_memory(self.namespace);
    }
}

/// Set the terminal's mouse pointer shape, or reset it to the terminal's own
/// default.
///
/// The terminal draws a pointer better than the compositor can. Clients state
/// the shape they want through `wp_cursor_shape_manager_v1`.
pub fn set_pointer_shape(out: &mut Vec<u8>, shape: Option<&str>) {
    match shape {
        Some(shape) => {
            let _ = write!(out, "\x1b]22;{shape}\x1b\\");
        }
        None => out.extend_from_slice(b"\x1b]22;\x1b\\"),
    }
}

/// The escape that sets the terminal's pointer shape, or resets it to the
/// terminal's own default.
pub fn pointer_shape_bytes(shape: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    set_pointer_shape(&mut out, shape);
    out
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
        // The terminal's shape set has no name for these, so they share the
        // nearest one.
        CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::AllResize => "move",
        _ => "default",
    }
}

/// Move the cursor to a cell. The coordinates are 1-based, as in the escape.
pub fn cursor_to(out: &mut Vec<u8>, col: u32, row: u32) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

/// Delete every image, which frees the terminal's memory for them.
pub fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

/// The escapes that wipe the screen, every image and every cell.
///
/// The server sends these to a pane after a resize, where stale pixels and
/// stale cell contents cannot be told apart from live ones. The bytes are
/// returned because the presenter writes them, after the frames already
/// promised.
pub fn clear() -> Vec<u8> {
    let mut out = Vec::new();
    delete_all(&mut out);
    // `CSI 2J` also drops every image the terminal holds, and homes the cursor.
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    out
}

/// The escape that names the window a terminal is in.
///
/// A client supplies the title, so control characters are removed: they would
/// let the client write escapes into the terminal that shows it. The title is
/// also cut to `MAXIMUM_TITLE`.
pub fn title(title: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(title.len() + 8);
    out.extend_from_slice(b"\x1b]2;");
    out.extend(
        title
            .chars()
            .filter(|c| !c.is_control())
            .take(MAXIMUM_TITLE)
            .collect::<String>()
            .bytes(),
    );
    out.push(b'\x07');
    out
}

/// How much of a client's title a terminal is told.
const MAXIMUM_TITLE: usize = 256;

/// Transmit a pane's whole frame as one image, at the top left of the screen.
///
/// One command transmits and places, which keeps an update cheap and flicker
/// free: the image id is replaced atomically, so the terminal goes from the old
/// frame to the new one. `C=1` keeps the cursor still, so a placement never
/// scrolls the terminal.
impl Encoder {
    /// Start a frame. The terminal buffers everything until
    /// [`Encoder::end_frame`], so a frame does not tear.
    pub fn begin_frame(out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026h");
    }

    /// End a frame: the terminal shows it.
    pub fn end_frame(out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026l");
    }

    /// Compress, if it pays, and send a screen of `pixels` to the terminal.
    pub fn transmit(&mut self, out: &mut Vec<u8>, pixels: &[u8], size: (u32, u32)) {
        debug_assert_eq!(
            pixels.len(),
            size.0 as usize * size.1 as usize * crate::render::BYTES,
            "a frame's pixels are its size"
        );
        // The image is placed where the cursor is, at its own pixel size: the
        // cursor is homed, so that is the top left of the screen.
        cursor_to(out, 0, 0);

        // With shared memory the pixels stay off the pty, so compressing them
        // would be work the terminal did not ask for.
        if self.shared_memory {
            let namespace = self.namespace;
            let object = self.shared.get_or_insert_with(|| Shared::new(namespace, 0));
            transmit(
                out,
                &mut self.payload,
                pixels,
                size,
                false,
                Some(object),
                FRAME_ID,
            );
            return;
        }
        // The frame is compressed before the decision is known: that is what
        // the decision costs.
        let compressed_len = self.compress(pixels);
        let worth = compressed_len * COMPRESSION_RATIO.1 < pixels.len() * COMPRESSION_RATIO.0;
        let payload = if worth { self.zlib.get_ref() } else { pixels };
        transmit(out, &mut self.payload, payload, size, worth, None, FRAME_ID);
    }

    /// Send one rectangle of a frame over the image the terminal holds.
    ///
    /// A patch goes the pty's way, compressed when it pays: it is a fraction of
    /// a screen, so a shared memory object would cost more in opening and
    /// unlinking than it saves in copying.
    pub fn transmit_patch(&mut self, out: &mut Vec<u8>, patch: Patch<'_>) {
        debug_assert_eq!(
            patch.pixels.len(),
            patch.size.0 as usize * patch.size.1 as usize * crate::render::BYTES,
            "a patch's pixels are its size"
        );
        cursor_to(out, patch.cell.0, patch.cell.1);
        let compressed_len = self.compress(patch.pixels);
        let worth = compressed_len * COMPRESSION_RATIO.1 < patch.pixels.len() * COMPRESSION_RATIO.0;
        let payload = if worth {
            self.zlib.get_ref()
        } else {
            patch.pixels
        };
        transmit(
            out,
            &mut self.payload,
            payload,
            patch.size,
            worth,
            None,
            patch.id,
        );
    }

    fn compress(&mut self, pixels: &[u8]) -> usize {
        if self.finished {
            // The writer's buffer still holds the previous frame. Taking it
            // back makes a steady stream of frames one allocation.
            let mut reused = self
                .zlib
                .reset(Vec::new())
                .expect("resetting a Vec encoder cannot fail");
            reused.clear();
            *self.zlib.get_mut() = reused;
            self.finished = false;
        }
        self.zlib.get_mut().reserve(pixels.len() / 8);
        // Compositor output is mostly flat color, so compressing usually
        // shrinks a frame by an order of magnitude.
        self.zlib
            .write_all(pixels)
            .expect("writing to a Vec cannot fail");
        self.zlib.try_finish().expect("finishing a Vec cannot fail");
        self.finished = true;
        self.zlib.get_ref().len()
    }
}

/// Send `payload` in a shared memory object if the terminal reads those, and
/// base64'd through the pty otherwise.
fn transmit(
    out: &mut Vec<u8>,
    encoded: &mut Vec<u8>,
    payload: &[u8],
    size: (u32, u32),
    compressed: bool,
    shared: Option<&Shared>,
    id: ImageId,
) {
    if let Some(object) = shared {
        match object.write(payload) {
            Ok(()) => {}
            // The pty still works, so a frame is not failed over this.
            Err(err) => {
                tracing::debug!(
                    ?err,
                    name = %object.name,
                    "could not put a frame in shared memory"
                );
                return direct(out, encoded, payload, size, compressed, id);
            }
        }
        placed(out, size, compressed, "t=s", &object.encoded_name, id);
        return;
    }
    direct(out, encoded, payload, size, compressed, id);
}

/// Send the payload base64'd inside the escape, in chunks of `CHUNK`.
fn direct(
    out: &mut Vec<u8>,
    encoded: &mut Vec<u8>,
    payload: &[u8],
    size: (u32, u32),
    compressed: bool,
    id: ImageId,
) {
    let payload = encode_base64(encoded, payload);
    chunked(out, size, compressed, payload, id);
}

fn encode_base64<'a>(encoded: &'a mut Vec<u8>, payload: &[u8]) -> &'a [u8] {
    let encoded_len =
        base64::encoded_len(payload.len(), true).expect("a frame fits in address space");
    encoded.resize(encoded_len, 0);
    BASE64
        .encode_slice(payload, encoded)
        .expect("the payload buffer has the exact encoded size");
    encoded.as_slice()
}

fn chunked(out: &mut Vec<u8>, size: (u32, u32), compressed: bool, payload: &[u8], id: ImageId) {
    let mut chunks = payload.chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        out.extend_from_slice(b"\x1b_G");
        if first {
            describe(out, size, compressed, None, id);
            first = false;
        }
        out.extend_from_slice(if chunks.peek().is_some() {
            b"m=1;"
        } else {
            b"m=0;"
        });
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// One escape whose payload is a name, not pixels.
fn placed(
    out: &mut Vec<u8>,
    size: (u32, u32),
    compressed: bool,
    medium: &str,
    payload: &[u8],
    id: ImageId,
) {
    out.extend_from_slice(b"\x1b_G");
    describe(out, size, compressed, Some(medium), id);
    out.extend_from_slice(b";");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b\\");
}

/// The control data every transmission carries, whatever the medium.
///
/// `a=T` transmits the pixels and places them in one command, `f=24` says they
/// are RGB, `o=z` says they are zlib compressed, and `t=s` says the payload is
/// a shared memory object name instead of pixels. `s` and `v` are the size in
/// pixels, `i` and `p` name the image and its placement, `z` the z-index, `C=1`
/// leaves the cursor still, and `q=2` suppresses the reply.
///
/// No `c` or `r` is sent: a cell rectangle would scale the frame, and a
/// terminal whose cell size does not divide the pane's pixels would scale it
/// away from the screen.
fn describe(
    out: &mut Vec<u8>,
    size: (u32, u32),
    compressed: bool,
    medium: Option<&str>,
    id: ImageId,
) {
    let (width, height) = size;
    let compression = if compressed { "o=z," } else { "" };
    let medium = medium.unwrap_or("");
    let separator = if medium.is_empty() { "" } else { "," };
    let _ = write!(
        out,
        "a=T,f=24,{compression}{medium}{separator}s={width},v={height},i={id},p={id},"
    );
    let _ = write!(out, "C=1,z={Z_ABOVE_TEXT},q=2,");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoding of one escape, independent of the encoder, so a wrong chunk
    /// size or byte order fails.
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
        Delete {
            id: u32,
            kind: String,
        },
    }

    fn decode(stream: &[u8]) -> Vec<Command> {
        let text = std::str::from_utf8(stream).expect("escapes are ASCII");
        let mut commands = Vec::new();
        let mut rest = text;
        // Partial transmissions are reassembled before they are interpreted:
        // the header comes from the first chunk, the payload from all of them.
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

    /// Interpret one reassembled graphics command. A command can transmit and
    /// place at once.
    fn interpret(header: &str, payload: &str) -> Vec<Command> {
        let action = field(header, "a");
        let mut commands = Vec::new();
        if matches!(action, Some("t" | "T")) {
            assert_eq!(field(header, "f"), Some("24"), "we only send RGB");
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
                width as usize * height as usize * crate::render::BYTES,
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
            commands.push(match field(header, "d") {
                Some("A") => Command::DeleteAll,
                kind => Command::Delete {
                    id: number(header, "i", 0),
                    kind: kind.unwrap_or("a").to_owned(),
                },
            });
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
                pixels.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8]);
            }
        }
        pixels
    }

    #[test]
    fn a_frame_round_trips_pixels_exactly() {
        let (width, height) = (64, 48);
        let pixels = test_pixels(width, height);
        let mut out = Vec::new();
        Encoder::new().transmit(&mut out, &pixels, (width, height));

        // This test covers the pixels surviving the trip, not the decision.
        let decoded = decode(&out);
        let (transmitted, put) = match decoded.as_slice() {
            [
                Command::Transmit {
                    id,
                    width: sent_width,
                    height: sent_height,
                    pixels: sent,
                    ..
                },
                Command::Put {
                    id: put_id,
                    placement,
                    cols,
                    rows,
                    moves_cursor: false,
                },
            ] => (
                (*id, *sent_width, *sent_height, sent),
                (*put_id, *placement, *cols, *rows),
            ),
            other => panic!("expected a transmission and its placement, got {other:?}"),
        };
        let id = FRAME_ID.into_inner();
        assert_eq!(
            (transmitted.0, transmitted.1, transmitted.2),
            (id, width, height)
        );
        // The frame keeps its own pixel size: no cell rectangle scales it.
        assert_eq!(put, (id, id, 0, 0));
        assert_eq!(transmitted.3, &pixels);
        let text = std::str::from_utf8(&out).expect("graphics escapes are ASCII");
        assert!(!text.contains(",c=") && !text.contains(",r="));
    }

    #[test]
    fn an_encoder_can_be_reused() {
        // Flat content, so this covers reusing the encoder only.
        let flat: Vec<u8> = [1u8, 2, 3].repeat(16);
        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        encoder.transmit(&mut out, &flat, (4, 4));
        out.clear();
        let flat: Vec<u8> = [4u8, 5, 6].repeat(16);
        encoder.transmit(&mut out, &flat, (4, 4));

        let id = FRAME_ID.into_inner();
        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id,
                    width: 4,
                    height: 4,
                    compressed: true,
                    pixels: [4u8, 5, 6].repeat(16),
                },
                Command::Put {
                    id,
                    placement: id,
                    cols: 0,
                    rows: 0,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn every_frame_carries_its_own_pixels() {
        // A frame's compression says nothing about the next one's bytes.
        let flat: Vec<u8> = [7u8, 8, 9].repeat(16);
        let other: Vec<u8> = [10u8, 11, 12].repeat(16);

        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        for pixels in [&flat, &other] {
            encoder.transmit(&mut out, pixels, (4, 4));
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
        let name = format!("/meowland-{}-1-1", std::process::id());
        let object = Shared::new(1, 1);
        let mut out = Vec::new();
        placed(
            &mut out,
            (160, 160),
            true,
            "t=s",
            &object.encoded_name,
            FRAME_ID,
        );

        // The name is the payload, so nothing else comes after the separator.
        let text = std::str::from_utf8(&out).expect("escapes are ascii");
        let (head, payload) = text
            .trim_start_matches("\x1b_G")
            .trim_end_matches("\x1b\\")
            .split_once(';')
            .expect("the escape separates control data from its payload");
        assert!(head.contains("t=s"), "{head}");
        // A POSIX shared memory name begins with a slash, and a terminal that
        // checks refuses one without it.
        assert!(name.starts_with('/'), "{name}");
        // The name travels base64'd, like every other payload in this protocol.
        assert_eq!(
            payload,
            String::from_utf8(object.encoded_name).expect("base64 is ascii")
        );
    }

    #[test]
    fn content_that_does_not_shrink_is_sent_as_it_is() {
        // Nothing for zlib to find: compressing would spend milliseconds to
        // save a fraction of what base64 adds back.
        let mut state = 0x1234_5678u32;
        let noise: Vec<u8> = (0..160 * 160 * crate::render::BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();

        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        encoder.transmit(&mut out, &noise, (160, 160));

        let id = FRAME_ID.into_inner();
        let commands = decode(&out);
        assert_eq!(
            commands,
            vec![
                Command::Transmit {
                    id,
                    width: 160,
                    height: 160,
                    compressed: false,
                    pixels: noise,
                },
                Command::Put {
                    id,
                    placement: id,
                    cols: 0,
                    rows: 0,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn a_frame_reads_out_of_shared_memory_when_the_terminal_can() {
        let mut encoder = Encoder::new();
        encoder.shared_memory = true;
        let pixels = test_pixels(16, 8);
        let mut out = Vec::new();
        encoder.transmit(&mut out, &pixels, (16, 8));

        // The escape names an object, and the pixels are in it.
        let text = std::str::from_utf8(&out).expect("escapes are ASCII");
        assert!(text.contains("t=s"), "{text}");
        let (_, payload) = text
            .trim_start_matches("\x1b[1;1H\x1b_G")
            .trim_end_matches("\x1b\\")
            .split_once(';')
            .expect("the escape separates control data from its payload");
        let name = String::from_utf8(BASE64.decode(payload).expect("the name is base64"))
            .expect("the name is text");
        assert_eq!(
            name,
            format!("/meowland-{}-{}-0", std::process::id(), encoder.namespace)
        );
        let path = format!("{SHM_DIRECTORY}{name}");
        assert_eq!(
            std::fs::read(&path).expect("the object holds the frame"),
            pixels
        );

        // The object is the encoder's, and goes with it.
        drop(encoder);
        assert!(
            !std::path::Path::new(&path).exists(),
            "{path} is left behind"
        );
    }

    #[test]
    fn every_chunk_is_a_whole_base64_line() {
        // Noise, so zlib cannot shrink the payload below one chunk.
        let (width, height) = (256, 256);
        let mut pixels =
            Vec::with_capacity(width as usize * height as usize * crate::render::BYTES);
        let mut state = 0x1234_5678u32;
        for _ in 0..width * height {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.extend_from_slice(&state.to_le_bytes()[..crate::render::BYTES]);
        }
        let mut out = Vec::new();
        Encoder::new().transmit(&mut out, &pixels, (width, height));

        let mut rest = std::str::from_utf8(&out).unwrap();
        let mut chunks = 0;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").unwrap();
            let (control, payload) = body[..end].split_once(';').unwrap();
            assert!(payload.len() <= CHUNK, "chunk longer than 4096 bytes");
            assert_eq!(payload.len() % 4, 0, "chunks must be whole base64 quanta");
            if control.contains("m=1") {
                // A chunk with a successor must be full, or the
                // concatenation cannot be decoded.
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
    fn a_patch_is_placed_in_the_cell_it_belongs_to() {
        // A rectangle of the screen goes as its own image: the cursor is moved
        // to the cell its top-left pixel is in, and nothing scales it.
        let (width, height) = (16, 8);
        let pixels = test_pixels(width, height);
        let id = ImageId::new(5);
        let mut out = Vec::new();
        Encoder::new().transmit_patch(
            &mut out,
            Patch {
                id,
                cell: (3, 2),
                size: (width, height),
                pixels: &pixels,
            },
        );

        let text = std::str::from_utf8(&out).expect("escapes are ASCII");
        assert!(text.starts_with("\x1b[3;4H"), "{text}");
        assert!(!text.contains(",c=") && !text.contains(",r="));
        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id: id.into_inner(),
                    width,
                    height,
                    compressed: true,
                    pixels,
                },
                Command::Put {
                    id: id.into_inner(),
                    placement: id.into_inner(),
                    cols: 0,
                    rows: 0,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn deleting_an_image_frees_its_pixels() {
        // A patch the terminal no longer shows is pixels it should not keep:
        // the capital form is the one that gives the data back.
        let mut out = Vec::new();
        delete_images(&mut out, &[ImageId::new(2), ImageId::new(7)]);
        assert_eq!(
            std::str::from_utf8(&out).expect("escapes are ASCII"),
            "\x1b_Ga=d,d=I,i=2,q=2;\x1b\\\x1b_Ga=d,d=I,i=7,q=2;\x1b\\"
        );
    }

    #[test]
    fn deleting_everything_is_a_single_escape() {
        let mut out = Vec::new();
        delete_all(&mut out);
        assert_eq!(decode(&out), vec![Command::DeleteAll]);
    }

    #[test]
    fn a_title_escape_carries_no_escapes_of_its_own() {
        let escape = title("\x1b]2;gotcha\x07\u{9b}31mred");
        assert_eq!(escape, b"\x1b]2;]2;gotcha31mred\x07");
        assert_eq!(title(""), b"\x1b]2;\x07");
        assert!(title(&"x".repeat(MAXIMUM_TITLE * 2)).len() < MAXIMUM_TITLE + 8);
    }

    #[test]
    fn wiping_the_screen_drops_every_image_first() {
        assert_eq!(clear(), b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[2J\x1b[H");
    }

    #[test]
    fn tilde_the_cursor_is_addressed_one_based() {
        let mut out = Vec::new();
        cursor_to(&mut out, 0, 0);
        cursor_to(&mut out, 3, 11);
        assert_eq!(out, b"\x1b[1;1H\x1b[12;4H");
    }
}
