//! Encoding of RGB frames for the kitty graphics protocol.
//!
//! `Presenter` deliberately owns the previous frame.  Callers can therefore
//! hand it a frame and immediately reuse their frame storage after this
//! method returns.  The output is one synchronized terminal update.

use std::{
    fs::File,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use rustix::{fs::Mode, shm};

const MAX_PATCHES: usize = 32;
const CHUNK: usize = 4096;
const SCREEN_ID: u32 = 1;
const FIRST_PATCH_ID: u32 = 2;
/// The image id of the one-pixel query that discovers shared-memory support.
pub const PROBE_ID: u32 = 32;

/// A POSIX shared memory slot a whole frame can be handed over in.
///
/// A terminal that reads one unlinks it, so a name that still resolves means
/// the previous frame has not been read: that frame goes over the pty instead,
/// which keeps the two transfers in the order they were made.
#[derive(Debug)]
pub struct SharedMemory {
    name: String,
}

impl SharedMemory {
    #[must_use]
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "/meowland-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let slot = Self { name };
        slot.clear();
        slot
    }

    /// Ask the terminal to read a one-pixel object, which is how the shared
    /// memory path is discovered.  Absent when the object cannot be made.
    #[must_use]
    pub fn probe(&self) -> Option<Vec<u8>> {
        let mut file = self.create()?;
        file.write_all(&[0; 3]).ok()?;
        Some(
            format!(
                "\x1b_Ga=q,f=24,s=1,v=1,i={PROBE_ID},t=s;{}\x1b\\",
                STANDARD.encode(self.name.as_bytes())
            )
            .into_bytes(),
        )
    }

    /// Forget an object the terminal left behind.
    pub fn clear(&self) {
        let _ = shm::unlink(self.name.as_str());
    }

    /// A fresh object, or `None` while the previous frame is still unread.
    fn create(&self) -> Option<File> {
        shm::open(
            self.name.as_str(),
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .ok()
        .map(File::from)
    }

    /// Hand a whole frame over, or report that it has to go the pty's way.
    fn transfer(&self, out: &mut Vec<u8>, width: u32, height: u32, pixels: &[u8]) -> bool {
        let Some(mut file) = self.create() else {
            return false;
        };
        if file.write_all(pixels).is_err() {
            self.clear();
            return false;
        }
        let payload = STANDARD.encode(self.name.as_bytes());
        out.reserve(payload.len() + 96);
        out.extend_from_slice(
            format!("\x1b_G{},t=s;", transmit(SCREEN_ID, width, height, 0)).as_bytes(),
        );
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        true
    }
}

impl Default for SharedMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Stateful kitty image presenter.
#[derive(Debug, Default)]
pub struct Presenter {
    cell_size: Option<(u16, u16)>,
    previous: Option<Frame>,
    base: Option<Frame>,
    patch_count: usize,
    shared: Option<SharedMemory>,
}

#[derive(Debug)]
struct Frame {
    width: u32,
    height: u32,
    pixels: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Presenter {
    /// Construct a presenter.  Patches are enabled only when the terminal's
    /// reported cell dimensions are supplied, and whole frames go through
    /// `shared` when the terminal proved it reads shared memory.
    #[must_use]
    pub const fn new(cell_size: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            cell_size,
            previous: None,
            base: None,
            patch_count: 0,
            shared,
        }
    }

    /// Encode a complete frame or a bounded set of cell-aligned patches.
    ///
    /// The RGB buffer must contain exactly `width * height * 3` bytes.  An
    /// invalid buffer is ignored and returns an empty update.
    #[must_use]
    pub fn present(&mut self, width: u32, height: u32, rgb: Vec<u8>) -> Vec<u8> {
        let pixels = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|n| n.checked_mul(3));
        if pixels != Some(rgb.len()) || width == 0 || height == 0 {
            return Vec::new();
        }
        if self.previous.as_ref().is_some_and(|old| {
            old.width == width && old.height == height && old.pixels.as_slice() == rgb
        }) {
            return Vec::new();
        }

        let frame = Frame {
            width,
            height,
            pixels: Arc::new(rgb),
        };
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(b"\x1b[?2026h");

        // Superseded patches are always deleted before the next ones are
        // drawn, so the live patch ids are exactly `2 ..= patch_count + 1`.
        let rects = self
            .base
            .as_ref()
            .filter(|old| old.width == width && old.height == height)
            .and_then(|old| self.changed_rects(old, &frame));
        let patches = match rects {
            // The frame returned to the base image: only the patches that
            // covered the difference have to go.
            Some(rects) if rects.is_empty() => (self.patch_count > 0).then(Vec::new),
            Some(rects) if within_patch_budget(&rects, width, height) => Some(rects),
            _ => None,
        };

        if let Some(rects) = patches {
            for index in 0..self.patch_count {
                delete_image(
                    &mut out,
                    FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX),
                );
            }
            for (index, rect) in rects.iter().enumerate() {
                let id = FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX);
                let payload = extract(&frame, *rect);
                move_cursor(&mut out, rect.x, rect.y, self.cell_size);
                image(&mut out, id, rect.width, rect.height, &payload, true);
            }
            self.patch_count = rects.len();
        } else {
            // `2J` also destroys images, so the layer is wiped before the
            // replacement is transmitted.
            delete_all(&mut out);
            out.extend_from_slice(b"\x1b[2J\x1b[H");
            let shared = self
                .shared
                .as_ref()
                .is_some_and(|slot| slot.transfer(&mut out, width, height, &frame.pixels));
            if !shared {
                image(&mut out, SCREEN_ID, width, height, &frame.pixels, false);
            }
            self.patch_count = 0;
            self.base = Some(Frame {
                width,
                height,
                pixels: Arc::clone(&frame.pixels),
            });
        }

        out.extend_from_slice(b"\x1b[?2026l");
        self.previous = Some(frame);
        out
    }

    fn changed_rects(&self, old: &Frame, new: &Frame) -> Option<Vec<Rect>> {
        let (cell_w, cell_h) = self.cell_size?;
        let (cell_w, cell_h) = (u32::from(cell_w), u32::from(cell_h));
        if cell_w == 0 || cell_h == 0 {
            return None;
        }
        let cols = new.width.div_ceil(cell_w);
        let rows = new.height.div_ceil(cell_h);
        let mut rects: Vec<Rect> = Vec::new();
        for row in 0..rows {
            let y = row * cell_h;
            let h = cell_h.min(new.height - y);
            let row_start = y as usize * new.width as usize * 3;
            let row_end = (y + h) as usize * new.width as usize * 3;
            if old.pixels[row_start..row_end] == new.pixels[row_start..row_end] {
                continue;
            }
            let mut col = 0;
            while col < cols {
                let x = col * cell_w;
                let w = cell_w.min(new.width - x);
                if !different(old, new, x, y, w, h) {
                    col += 1;
                    continue;
                }
                let start = col;
                col += 1;
                while col < cols {
                    let next_x = col * cell_w;
                    let next_w = cell_w.min(new.width - next_x);
                    if !different(old, new, next_x, y, next_w, h) {
                        break;
                    }
                    col += 1;
                }
                let rect = Rect {
                    x: start * cell_w,
                    y,
                    width: ((col - start) * cell_w).min(new.width - start * cell_w),
                    height: h,
                };
                if !rect.width.is_multiple_of(cell_w) || !rect.height.is_multiple_of(cell_h) {
                    return None;
                }
                // Merge vertically adjacent equal-width runs.  This keeps
                // flat UI changes compact without expensive rectangle packing.
                if let Some(last) = rects.last_mut()
                    && last.x == rect.x
                    && last.width == rect.width
                    && last.y + last.height == rect.y
                {
                    last.height += rect.height;
                    continue;
                }
                rects.push(rect);
                if rects.len() > MAX_PATCHES {
                    return Some(rects);
                }
            }
        }
        Some(rects)
    }
}

fn different(old: &Frame, new: &Frame, x: u32, y: u32, width: u32, height: u32) -> bool {
    let old_stride = old.width as usize * 3;
    let new_stride = new.width as usize * 3;
    for row in y as usize..(y + height) as usize {
        let start = x as usize * 3;
        let end = (x + width) as usize * 3;
        if old.pixels[row * old_stride + start..row * old_stride + end]
            != new.pixels[row * new_stride + start..row * new_stride + end]
        {
            return true;
        }
    }
    false
}

/// A patch set is worth sending only while it stays small, stays under the
/// patch id budget and moves less pixel data than a whole frame would.
fn within_patch_budget(rects: &[Rect], width: u32, height: u32) -> bool {
    !rects.is_empty()
        && rects.len() <= MAX_PATCHES
        && rects
            .iter()
            .map(|rect| u64::from(rect.width) * u64::from(rect.height))
            .sum::<u64>()
            < u64::from(width) * u64::from(height)
}

fn extract(frame: &Frame, rect: Rect) -> Vec<u8> {
    let stride = frame.width as usize * 3;
    let row_len = rect.width as usize * 3;
    let mut out = Vec::with_capacity(row_len * rect.height as usize);
    for row in rect.y as usize..(rect.y + rect.height) as usize {
        let start = rect.x as usize * 3;
        out.extend_from_slice(&frame.pixels[row * stride + start..row * stride + start + row_len]);
    }
    out
}

fn move_cursor(out: &mut Vec<u8>, x: u32, y: u32, cell_size: Option<(u16, u16)>) {
    let (cell_w, cell_h) = cell_size.map_or((1, 1), |(w, h)| (u32::from(w), u32::from(h)));
    let col = x / cell_w + 1;
    let row = y / cell_h + 1;
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").as_bytes());
}

/// The keys every transmitted image shares, whichever way its pixels travel;
/// the caller appends `,t=s;`, the compression, or the chunk continuation.
fn transmit(id: u32, width: u32, height: u32, placement: u32) -> String {
    format!("a=T,f=24,s={width},v={height},i={id},p={placement},z=1,C=1,q=2")
}

fn image(out: &mut Vec<u8>, id: u32, width: u32, height: u32, pixels: &[u8], patch: bool) {
    let compressed = compress(pixels);
    let (payload, zlib) = compressed.as_deref().map_or((pixels, false), |candidate| {
        if candidate.len() * 4 <= pixels.len() * 3 {
            (candidate, true)
        } else {
            (pixels, false)
        }
    });
    let encoded = STANDARD.encode(payload);
    // Reserve once: growing to a whole 1080p frame in doublings copies it
    // about twenty times.
    out.reserve(encoded.len() + 128);
    let compression = if zlib { ",o=z" } else { "" };
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        // Every chunk but the last says `m=1`; a whole image in one chunk
        // carries no `m` key at all.
        let more = chunks.peek().is_some();
        if first {
            let marker = if more { ",m=1" } else { "" };
            let control = transmit(id, width, height, u32::from(patch));
            out.extend_from_slice(format!("\x1b_G{control}{compression}{marker};").as_bytes());
            first = false;
        } else if more {
            out.extend_from_slice(b"\x1b_Gm=1;");
        } else {
            out.extend_from_slice(b"\x1b_Gm=0;");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Compress `data`, or nothing when a sample says the full pass would not pay.
///
/// Compressing megabytes of already-compressed pixels costs more than a whole
/// frame's time budget, and the result would be thrown away, so four spread
/// samples decide first.
fn compress(data: &[u8]) -> Option<Vec<u8>> {
    const SAMPLE_BYTES: usize = 16 * 1024;
    const SAMPLES: usize = 4;
    if data.len() > SAMPLE_BYTES * SAMPLES {
        let mut sample = Vec::with_capacity(SAMPLE_BYTES * SAMPLES);
        for index in 0..SAMPLES {
            let start = (data.len() - SAMPLE_BYTES) * index / (SAMPLES - 1);
            sample.extend_from_slice(&data[start..start + SAMPLE_BYTES]);
        }
        if zlib(&sample)?.len() * 4 > sample.len() * 3 {
            return None;
        }
    }
    zlib(data)
}

fn zlib(data: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::Read};

    use super::*;

    /// One placement of transmitted image data, keyed the way kitty keys it.
    #[derive(Debug)]
    struct Placement {
        z: i32,
        image: u32,
        reference: u32,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    }

    /// Independent decoder for the emitted stream.  It models the image layer
    /// the terminal keeps: transmitted data, placements anchored to cell
    /// positions, z-ordering by `(z, image id, placement id)`, `a=d,d=A`
    /// freeing every image, `a=d,d=I` freeing one, and `2J` destroying the
    /// placements on screen.
    struct Replay {
        width: usize,
        height: usize,
        cell: (usize, usize),
        cursor: (usize, usize),
        images: HashMap<u32, Vec<u8>>,
        placements: HashMap<(u32, u32), Placement>,
        /// A terminal that has not looked at its input yet ignores a shared
        /// object transfer until it does.
        read_shared: bool,
    }

    impl Replay {
        fn new(width: usize, height: usize, cell: (usize, usize)) -> Self {
            Self {
                width,
                height,
                cell,
                cursor: (0, 0),
                images: HashMap::new(),
                placements: HashMap::new(),
                read_shared: true,
            }
        }

        fn feed(&mut self, stream: &[u8]) {
            let mut pos = 0;
            while pos < stream.len() {
                if let Some((final_byte, parameters, next)) = csi(stream, pos) {
                    match final_byte {
                        b'H' => self.cursor = cursor_from(parameters),
                        b'J' => self.placements.clear(),
                        _ => {}
                    }
                    pos = next;
                } else if stream[pos..].starts_with(b"\x1b_G") {
                    pos = self.command(stream, pos);
                } else {
                    pos += 1;
                }
            }
        }

        /// Consumes one graphics command, chunk continuations included, and
        /// returns the offset just past it.
        fn command(&mut self, stream: &[u8], pos: usize) -> usize {
            const ALLOWED: [&str; 12] =
                ["a", "f", "s", "v", "i", "p", "z", "C", "q", "o", "m", "t"];
            let end = escape_end(stream, pos + 3);
            let (parameters, first_chunk) = split_data(&stream[pos + 3..end]);
            let parameters = std::str::from_utf8(parameters).unwrap();
            let fields: HashMap<&str, &str> = parameters
                .split(',')
                .filter_map(|field| field.split_once('='))
                .collect();

            let mut encoded = first_chunk.to_vec();
            let mut more = fields.get("m") == Some(&"1");
            let mut next = end + 2;
            while more {
                assert!(
                    stream[next..].starts_with(b"\x1b_G"),
                    "a continuation chunk must be a graphics command"
                );
                let chunk_end = escape_end(stream, next + 3);
                let (chunk_parameters, data) = split_data(&stream[next + 3..chunk_end]);
                assert!(
                    chunk_parameters == b"m=1" || chunk_parameters == b"m=0",
                    "continuation chunks carry only the m key, got {:?}",
                    String::from_utf8_lossy(chunk_parameters)
                );
                encoded.extend_from_slice(data);
                more = chunk_parameters == b"m=1";
                next = chunk_end + 2;
            }

            match fields.get("a").copied() {
                Some("T") => {
                    if fields.get("t") == Some(&"s") && !self.read_shared {
                        return next;
                    }
                    for key in fields.keys() {
                        assert!(
                            ALLOWED.contains(key),
                            "unexpected key {key} in {parameters:?}"
                        );
                    }
                    for key in ["a", "f", "s", "v", "i", "p", "z", "C", "q"] {
                        assert!(
                            fields.contains_key(key),
                            "missing key {key} in {parameters:?}"
                        );
                    }
                    assert_eq!(fields["f"], "24", "RGB only");
                    assert_eq!(fields["C"], "1", "a placement must never move the cursor");
                    assert_eq!(fields["q"], "2", "replies must be suppressed");
                    assert_eq!(fields["z"], "1", "images draw above the text");
                    let id: u32 = fields["i"].parse().unwrap();
                    let reference: u32 = fields["p"].parse().unwrap();
                    let width: usize = fields["s"].parse().unwrap();
                    let height: usize = fields["v"].parse().unwrap();
                    assert!(
                        !self.images.contains_key(&id),
                        "image {id} was re-transmitted before being deleted"
                    );
                    let pixels = if fields.get("t") == Some(&"s") {
                        read_shared_object(&encoded)
                    } else {
                        inflate(&encoded, fields.get("o") == Some(&"z"))
                    };
                    assert_eq!(
                        pixels.len(),
                        width * height * 3,
                        "payload does not match s and v"
                    );
                    self.images.insert(id, pixels);
                    let (row, column) = self.cursor;
                    self.placements.insert(
                        (id, reference),
                        Placement {
                            z: fields["z"].parse().unwrap(),
                            image: id,
                            reference,
                            x: column * self.cell.0,
                            y: row * self.cell.1,
                            width,
                            height,
                        },
                    );
                }
                Some("d") => {
                    let what = fields.get("d").copied().unwrap_or("A");
                    if what == "A" {
                        self.placements.clear();
                        self.images.clear();
                    } else {
                        assert_eq!(what, "I", "only d=A and d=I are emitted");
                        let id: u32 = fields["i"].parse().unwrap();
                        self.placements.retain(|(image, _), _| *image != id);
                        self.images.remove(&id);
                    }
                }
                other => panic!("unexpected graphics action {other:?}"),
            }
            next
        }

        fn screen(&self) -> Vec<u8> {
            let mut screen = vec![0; self.width * self.height * 3];
            let mut order: Vec<&Placement> = self.placements.values().collect();
            order.sort_by_key(|placement| (placement.z, placement.image, placement.reference));
            for placement in order {
                let pixels = &self.images[&placement.image];
                for row in 0..placement.height {
                    let destination = ((placement.y + row) * self.width + placement.x) * 3;
                    let source = row * placement.width * 3;
                    screen[destination..destination + placement.width * 3]
                        .copy_from_slice(&pixels[source..source + placement.width * 3]);
                }
            }
            screen
        }
    }

    /// The terminal side of `t=s`: the object holds the pixels, and reading it
    /// is what unlinking it means.
    fn read_shared_object(encoded: &[u8]) -> Vec<u8> {
        let name = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
        let path = std::path::Path::new("/dev/shm").join(name.trim_start_matches('/'));
        let pixels = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        pixels
    }

    fn shared_memory() -> Option<SharedMemory> {
        let slot = SharedMemory::new();
        slot.probe()?;
        // No terminal is here to read and unlink the probe object, so it is
        // dropped here before the presenter takes the slot.
        slot.clear();
        Some(slot)
    }

    fn csi(stream: &[u8], pos: usize) -> Option<(u8, &str, usize)> {
        if !stream[pos..].starts_with(b"\x1b[") {
            return None;
        }
        let end = stream[pos + 2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))?
            + pos
            + 2;
        let parameters = std::str::from_utf8(&stream[pos + 2..end]).ok()?;
        Some((stream[end], parameters, end + 1))
    }

    fn cursor_from(parameters: &str) -> (usize, usize) {
        if parameters.is_empty() {
            return (0, 0);
        }
        let mut parts = parameters
            .split(';')
            .map(|part| part.parse::<usize>().unwrap());
        (parts.next().unwrap() - 1, parts.next().unwrap() - 1)
    }

    fn escape_end(stream: &[u8], start: usize) -> usize {
        start
            + stream[start..]
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .expect("every graphics command ends with ST")
    }

    fn split_data(body: &[u8]) -> (&[u8], &[u8]) {
        body.iter()
            .position(|byte| *byte == b';')
            .map_or((body, &[]), |index| (&body[..index], &body[index + 1..]))
    }

    fn inflate(encoded: &[u8], zlib: bool) -> Vec<u8> {
        let mut decoded = STANDARD.decode(encoded).unwrap();
        if zlib {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(decoded.as_slice())
                .read_to_end(&mut out)
                .unwrap();
            decoded = out;
        }
        decoded
    }

    /// Control strings of every graphics command in `stream`.
    fn commands(stream: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < stream.len() {
            if !stream[pos..].starts_with(b"\x1b_G") {
                pos += 1;
                continue;
            }
            let end = escape_end(stream, pos + 3);
            let (parameters, _) = split_data(&stream[pos + 3..end]);
            out.push(String::from_utf8_lossy(parameters).into_owned());
            pos = end + 2;
        }
        out
    }

    fn field<'a>(control: &'a str, key: &str) -> Option<&'a str> {
        control
            .split(',')
            .filter_map(|field| field.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }

    fn transmits(stream: &[u8]) -> Vec<String> {
        commands(stream)
            .into_iter()
            .filter(|control| field(control, "a") == Some("T"))
            .collect()
    }

    fn whole_frames(stream: &[u8]) -> Vec<String> {
        transmits(stream)
            .into_iter()
            .filter(|control| field(control, "p") == Some("0"))
            .collect()
    }

    fn patches(stream: &[u8]) -> Vec<String> {
        transmits(stream)
            .into_iter()
            .filter(|control| field(control, "p") == Some("1"))
            .collect()
    }

    fn replay(streams: &[Vec<u8>], width: usize, height: usize, cell: (usize, usize)) -> Vec<u8> {
        let mut replay = Replay::new(width, height, cell);
        for stream in streams {
            replay.feed(stream);
        }
        replay.screen()
    }

    fn next(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    fn noise(state: &mut u32, len: usize) -> Vec<u8> {
        (0..len).map(|_| next(state) as u8).collect()
    }

    /// Fills the `w` by `h` pixel block at `(x, y)` with one value.
    fn put_block(frame: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32, value: u8) {
        for row in 0..h {
            let start = ((y + row) * width + x) as usize * 3;
            frame[start..start + (w * 3) as usize].fill(value);
        }
    }

    #[test]
    fn first_frame_is_whole_and_a_cell_change_is_a_patch() {
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = noise(&mut 1, 4 * 2 * 3);
        let first = presenter.present(4, 2, base.clone());
        assert_eq!(whole_frames(&first).len(), 1);
        assert_eq!(patches(&first), Vec::<String>::new());

        let mut changed = base;
        put_block(&mut changed, 4, 0, 0, 2, 2, 0xab);
        let second = presenter.present(4, 2, changed.clone());
        assert_eq!(patches(&second).len(), 1);
        assert_eq!(whole_frames(&second).len(), 0);
        assert_eq!(replay(&[first, second], 4, 2, (2, 2)), changed);
    }

    #[test]
    fn unchanged_frame_is_empty() {
        let mut presenter = Presenter::new(None, None);
        let rgb = vec![1; 12];
        assert_ne!(presenter.present(2, 2, rgb.clone()), Vec::<u8>::new());
        assert_eq!(presenter.present(2, 2, rgb), Vec::<u8>::new());
    }

    #[test]
    fn whole_frame_transmit_has_exact_parameters() {
        let mut presenter = Presenter::new(None, None);
        let update = presenter.present(2, 2, noise(&mut 7, 2 * 2 * 3));
        assert!(
            update.starts_with(b"\x1b[?2026h\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[2J\x1b[H"),
            "whole frames wipe the layer, clear the screen and home the cursor"
        );
        assert!(update.ends_with(b"\x1b[?2026l"));
        assert_eq!(
            whole_frames(&update)[0],
            "a=T,f=24,s=2,v=2,i=1,p=0,z=1,C=1,q=2"
        );

        let mut sequences = Vec::new();
        let mut pos = 0;
        while pos < update.len() {
            if let Some((final_byte, parameters, next)) = csi(&update, pos) {
                sequences.push(format!("{parameters}{}", char::from(final_byte)));
                pos = next;
            } else {
                pos += 1;
            }
        }
        assert_eq!(
            sequences,
            ["?2026h", "2J", "H", "?2026l"],
            "nothing else may move the cursor or scroll the terminal"
        );
    }

    #[test]
    fn patch_is_placed_by_the_cursor_alone() {
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 4 * 4 * 3];
        let first = presenter.present(4, 4, base.clone());
        let mut changed = base;
        let cell = noise(&mut 3, 2 * 2 * 3);
        for row in 0..2 {
            let start = ((2 + row) * 4 + 2) * 3;
            changed[start..start + 6].copy_from_slice(&cell[row * 6..row * 6 + 6]);
        }
        let second = presenter.present(4, 4, changed.clone());

        assert!(String::from_utf8_lossy(&second).contains("\x1b[2;2H"));
        let patch = &patches(&second)[0];
        let mut keys: Vec<&str> = patch
            .split(',')
            .filter_map(|field| field.split_once('=').map(|(key, _)| key))
            .collect();
        keys.retain(|key| *key != "o");
        assert_eq!(keys, ["a", "f", "s", "v", "i", "p", "z", "C", "q"]);
        assert_eq!(field(patch, "s"), Some("2"));
        assert_eq!(field(patch, "v"), Some("2"));
        assert_eq!(field(patch, "i"), Some("2"));
        let text = String::from_utf8_lossy(&second);
        assert!(
            !text.contains(",c=") && !text.contains(",r="),
            "patches are never scaled"
        );
        assert_eq!(replay(&[first, second], 4, 4, (2, 2)), changed);
    }

    #[test]
    fn patches_need_a_whole_base_of_the_same_size() {
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 4 * 4 * 3];
        let first = presenter.present(4, 4, base.clone());
        let mut changed = base;
        put_block(&mut changed, 4, 0, 0, 2, 2, 200);
        let second = presenter.present(4, 4, changed.clone());
        assert_eq!(patches(&second).len(), 1);

        let mut larger = vec![0; 6 * 4 * 3];
        put_block(&mut larger, 6, 4, 0, 2, 2, 90);
        let third = presenter.present(6, 4, larger.clone());
        assert!(
            patches(&third).is_empty(),
            "a new size cannot patch the old base"
        );
        assert!(String::from_utf8_lossy(&third).contains("a=d,d=A"));
        let transmit = &whole_frames(&third)[0];
        assert_eq!(field(transmit, "s"), Some("6"));
        assert_eq!(field(transmit, "v"), Some("4"));
        assert_eq!(replay(&[first, second, third], 6, 4, (2, 2)), larger);
    }

    #[test]
    fn returning_to_the_base_deletes_patches_without_resending_it() {
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = noise(&mut 11, 4 * 4 * 3);
        let first = presenter.present(4, 4, base.clone());
        let mut changed = base.clone();
        put_block(&mut changed, 4, 2, 2, 2, 2, 255);
        let second = presenter.present(4, 4, changed);
        assert_eq!(patches(&second).len(), 1);

        let third = presenter.present(4, 4, base.clone());
        assert!(
            transmits(&third).is_empty(),
            "the base image still is on screen"
        );
        assert!(
            commands(&third).iter().any(|c| c == "a=d,d=I,i=2,q=2"),
            "the superseded patch is deleted"
        );
        assert_eq!(replay(&[first, second, third], 4, 4, (2, 2)), base);
    }

    #[test]
    fn a_smaller_patch_set_deletes_the_superseded_ids() {
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 4 * 4 * 3];
        let first = presenter.present(4, 4, base.clone());
        let mut two = base.clone();
        put_block(&mut two, 4, 0, 0, 2, 2, 30);
        put_block(&mut two, 4, 2, 2, 2, 2, 60);
        let second = presenter.present(4, 4, two.clone());
        assert_eq!(patches(&second).len(), 2);

        let mut one = base;
        put_block(&mut one, 4, 0, 0, 2, 2, 30);
        let third = presenter.present(4, 4, one.clone());
        assert_eq!(patches(&third).len(), 1);
        assert!(
            commands(&third).iter().any(|c| c == "a=d,d=I,i=3,q=2"),
            "unused patch ids are freed before redrawing"
        );
        assert_eq!(replay(&[first, second, third], 4, 4, (2, 2)), one);
    }

    #[test]
    fn changes_beyond_the_patch_budget_go_whole() {
        let runs = MAX_PATCHES as u32 + 1;
        // One changed cell, then one untouched cell, per run.
        let width = runs * 4 + 2;
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = vec![0; width as usize * 2 * 3];
        let first = presenter.present(width, 2, base.clone());
        let mut scattered = base;
        for run in 0..runs {
            put_block(&mut scattered, width, run * 4, 0, 2, 2, 100);
        }
        let second = presenter.present(width, 2, scattered.clone());
        assert!(patches(&second).is_empty(), "{runs} runs exceed the limit");
        assert_eq!(
            replay(&[first, second], width as usize, 2, (2, 2)),
            scattered
        );

        let mut coverage = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 4 * 4 * 3];
        let first = coverage.present(4, 4, base);
        let full = vec![200; 4 * 4 * 3];
        let second = coverage.present(4, 4, full.clone());
        assert!(patches(&second).is_empty(), "a full frame is not a patch");
        assert_eq!(replay(&[first, second], 4, 4, (2, 2)), full);

        let mut unaligned = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 5 * 2 * 3];
        let first = unaligned.present(5, 2, base.clone());
        let mut ragged = base;
        put_block(&mut ragged, 5, 4, 0, 1, 2, 77);
        let second = unaligned.present(5, 2, ragged.clone());
        assert!(
            patches(&second).is_empty(),
            "a partial cell cannot be patched"
        );
        assert_eq!(replay(&[first, second], 5, 2, (2, 2)), ragged);
    }

    #[test]
    fn payload_chunks_respect_the_protocol_framing() {
        let mut presenter = Presenter::new(None, None);
        let update = presenter.present(100, 100, noise(&mut 5, 30_000));

        let mut chunks: Vec<(String, usize)> = Vec::new();
        let mut pos = 0;
        while pos < update.len() {
            if !update[pos..].starts_with(b"\x1b_G") {
                pos += 1;
                continue;
            }
            let end = escape_end(&update, pos + 3);
            let (parameters, data) = split_data(&update[pos + 3..end]);
            chunks.push((String::from_utf8_lossy(parameters).into_owned(), data.len()));
            pos = end + 2;
        }
        assert!(chunks.len() > 5, "30k of noise cannot fit in one chunk");
        let first = chunks
            .iter()
            .position(|(parameters, _)| parameters.starts_with("a=T"))
            .expect("a transmit command");
        assert!(chunks[first].0.ends_with(",m=1"), "{:?}", chunks[first].0);
        for (index, (parameters, length)) in chunks.iter().enumerate().skip(first + 1) {
            let last = index + 1 == chunks.len();
            assert_eq!(parameters, if last { "m=0" } else { "m=1" });
            assert!(*length <= CHUNK, "chunks are bounded by CHUNK");
            if !last {
                assert_eq!(length % 4, 0, "chunks are whole base64 quanta");
            }
        }
    }

    #[test]
    fn randomized_frame_sequences_replay_exactly() {
        const WIDTH: usize = 4;
        const HEIGHT: usize = 4;
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let mut decoder = Replay::new(WIDTH, HEIGHT, (2, 2));
        let mut state = 0x1234_5678_u32;
        let mut frame = vec![0; WIDTH * HEIGHT * 3];
        let mut base = frame.clone();
        let (mut saw_whole, mut saw_patch, mut saw_deletes) = (false, false, false);
        for step in 0..400 {
            match step % 11 {
                0 => {}
                1 => frame.clone_from(&base),
                5 => {
                    for cell in 0..4 {
                        put_block(
                            &mut frame,
                            WIDTH as u32,
                            (cell % 2) * 2,
                            (cell / 2) * 2,
                            2,
                            2,
                            next(&mut state) as u8,
                        );
                    }
                }
                _ => {
                    for _ in 0..=next(&mut state) % 2 {
                        let cell = next(&mut state) % 4;
                        let (x, y) = ((cell % 2) * 2, (cell / 2) * 2);
                        for row in 0..2 {
                            let start = ((y + row) * WIDTH as u32 + x) as usize * 3;
                            frame[start..start + 6].copy_from_slice(&noise(&mut state, 6));
                        }
                    }
                }
            }
            let update = presenter.present(WIDTH as u32, HEIGHT as u32, frame.clone());
            let controls = commands(&update);
            let transmit = |kind: &str| {
                controls.iter().any(|control| {
                    field(control, "a") == Some("T") && field(control, "p") == Some(kind)
                })
            };
            saw_whole |= transmit("0");
            saw_patch |= transmit("1");
            saw_deletes |= !update.is_empty() && !transmit("0") && !transmit("1");
            if transmit("0") {
                base.clone_from(&frame);
            }
            decoder.feed(&update);
            assert_eq!(decoder.screen(), frame, "screen diverged at step {step}");
        }
        assert!(
            saw_whole && saw_patch && saw_deletes,
            "the sequence must exercise whole frames, patches and deletes"
        );
    }

    #[test]
    fn a_whole_frame_goes_through_shared_memory_when_the_terminal_reads_it() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let frame = noise(&mut 11, 4 * 2 * 3);
        let update = presenter.present(4, 2, frame.clone());
        let control = whole_frames(&update);
        assert_eq!(control.len(), 1);
        assert!(
            control[0].contains("t=s"),
            "not a shared transfer: {control:?}"
        );
        assert_eq!(replay(&[update], 4, 2, (2, 2)), frame);
    }

    #[test]
    fn a_slot_the_terminal_has_not_read_goes_over_the_pty_instead() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let mut replay = Replay::new(4, 4, (2, 2));
        replay.read_shared = false;
        replay.feed(&presenter.present(4, 2, noise(&mut 13, 4 * 2 * 3)));

        let second = noise(&mut 17, 4 * 4 * 3);
        let update = presenter.present(4, 4, second.clone());
        let control = whole_frames(&update);
        assert_eq!(control.len(), 1);
        assert!(
            !control[0].contains("t=s"),
            "the pty path was needed: {control:?}"
        );
        replay.read_shared = true;
        replay.feed(&update);
        assert_eq!(replay.screen(), second);
    }

    #[test]
    fn the_shared_memory_probe_asks_for_one_pixel_through_an_object() {
        let slot = SharedMemory::new();
        let Some(probe) = slot.probe() else {
            return;
        };
        let text = String::from_utf8_lossy(&probe);
        assert!(text.starts_with("\x1b_Ga=q,f=24,s=1,v=1,"), "{text:?}");
        assert!(text.contains(&format!("i={PROBE_ID}")), "{text:?}");
        assert!(text.contains("t=s;"), "{text:?}");
        let path = format!("/dev/shm/{}", slot.name.trim_start_matches('/'));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3);
        slot.clear();
        assert!(!std::path::Path::new(&path).exists());
    }
}
