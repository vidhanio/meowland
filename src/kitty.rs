//! Encode RGB frames for the kitty graphics protocol.

use std::{
    fs::File,
    io::Write,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use rustix::{fs::Mode, shm};

const MAX_PATCHES: usize = 32;
/// Consecutive unread frames before abandoning shared memory permanently.
const DROP_LIMIT: u32 = 30;
/// Most of a frame a cell-aligned diff may cover while a shared slot is
/// available: past this, one whole transfer into the object costs less than
/// megabytes of patches on the pty.
const SHARED_PATCH_DIVISOR: u64 = 4;
const CHUNK: usize = 4096;
/// Base64 payload bytes per 4096-character chunk.
const CHUNK_PAYLOAD: usize = CHUNK / 4 * 3;
const SCREEN_ID: u32 = 1;
const FIRST_PATCH_ID: u32 = 2;
/// Image ID reserved for the shared-memory capability probe.
pub const PROBE_ID: u32 = 32;

/// POSIX shared-memory slot for whole frames. A terminal unlinks each object
/// after reading it; persistent unread objects disable this path.
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

    /// Query shared-memory support with a one-pixel object, if creation
    /// succeeds.
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

    pub fn clear(&self) {
        let _ = shm::unlink(self.name.as_str());
    }

    fn create(&self) -> Option<File> {
        shm::open(
            self.name.as_str(),
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .ok()
        .map(File::from)
    }

    fn transfer(
        &self,
        out: &mut Vec<u8>,
        mut file: File,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> bool {
        if file.write_all(pixels).is_err() {
            self.clear();
            return false;
        }
        let payload = STANDARD.encode(self.name.as_bytes());
        out.reserve(payload.len() + 96);
        out.extend_from_slice(b"\x1b_G");
        transmit(out, SCREEN_ID, width, height, 0);
        out.extend_from_slice(b",t=s;");
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

/// Retains composed frames to encode changed bands as whole images or patches.
#[derive(Debug, Default)]
pub struct Presenter {
    cell_size: Option<(u16, u16)>,
    /// Composed pixels; full-frame input transfers ownership of its allocation.
    image: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    /// Base under patches, or a shared frame awaiting confirmation.
    base: Option<Frame>,
    patch_count: usize,
    /// Whether `image` is already displayed.
    display_current: bool,
    rects: Vec<Rect>,
    /// Rows that may differ from the whole-image base, including live patches.
    damage: Range<u32>,
    shared: Option<SharedMemory>,
    encoder: EncodingBuffers,
    patch_pixels: Vec<u8>,
    dropped: bool,
    drops: u32,
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

#[derive(Debug, Default)]
struct EncodingBuffers {
    compressed: Vec<u8>,
    sample: Vec<u8>,
}

impl Presenter {
    /// Enable patches with known cell dimensions and shared frames with a
    /// terminal-confirmed shared-memory slot.
    #[must_use]
    pub fn new(cell_size: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            cell_size,
            image: Arc::new(Vec::new()),
            width: 0,
            height: 0,
            base: None,
            display_current: false,
            rects: Vec::new(),
            damage: 0..0,
            patch_count: 0,
            shared,
            encoder: EncodingBuffers::default(),
            patch_pixels: Vec::new(),
            dropped: false,
            drops: 0,
        }
    }

    /// Changing the grid invalidates displayed patches.
    pub fn set_cell_size(&mut self, cell_size: Option<(u16, u16)>) {
        if self.cell_size != cell_size {
            self.cell_size = cell_size;
            self.base = None;
            self.patch_count = 0;
            self.display_current = false;
        }
    }

    /// Whether the latest update was dropped while shared memory remained
    /// unread.
    #[must_use]
    pub const fn dropped(&self) -> bool {
        self.dropped
    }

    /// Compose a changed row band and encode its terminal update.
    ///
    /// Out-of-bounds or malformed bands leave the frame unchanged.
    #[must_use]
    pub fn present(&mut self, width: u32, height: u32, y: u32, band: Vec<u8>) -> Vec<u8> {
        self.dropped = false;
        let Some(changed) = self.apply_band(width, height, y, band) else {
            return Vec::new();
        };
        if !changed && self.display_current {
            return Vec::new();
        }

        // Without patches, restoring the old base after a dropped frame needs
        // no transfer: it is still on screen.
        if self.patch_count == 0
            && self.base.as_ref().is_some_and(|old| {
                old.width == width
                    && old.height == height
                    && (Arc::ptr_eq(&old.pixels, &self.image) || {
                        let stride = width as usize * 3;
                        let rows =
                            self.damage.start as usize * stride..self.damage.end as usize * stride;
                        old.pixels[rows.clone()] == self.image[rows]
                    })
            })
        {
            self.drops = 0;
            self.display_current = true;
            return Vec::new();
        }

        // Replaced patches are deleted; live IDs span `2 ..= patch_count + 1`.
        let patches = self.select_patches(width, height);

        let file = if patches {
            None
        } else {
            match self.ready_shared() {
                Ok(file) => file,
                Err(()) => return Vec::new(),
            }
        };

        // Size inline output for the selected compressed or raw payload.
        let prepared =
            (!patches && file.is_none()).then(|| prepare_image(&mut self.encoder, &self.image));
        let capacity = prepared.map_or(64, |compressed| {
            let bytes = if compressed {
                self.encoder.compressed.len()
            } else {
                self.image.len()
            };
            encoded_capacity(bytes) + 64
        });
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(b"\x1b[?2026h");

        if patches {
            self.write_patches(&mut out, width);
        } else {
            self.write_whole(&mut out, width, height, file, prepared);
        }

        out.extend_from_slice(b"\x1b[?2026l");
        self.display_current = true;
        out
    }

    /// Drop frames while the terminal is briefly behind, then permanently
    /// fall back to pty transfer rather than cycling through freezes.
    fn ready_shared(&mut self) -> Result<Option<File>, ()> {
        let Some(slot) = &self.shared else {
            return Ok(None);
        };
        let file = slot.create();
        if file.is_some() {
            self.drops = 0;
            return Ok(file);
        }
        self.drops += 1;
        if self.drops < DROP_LIMIT {
            self.dropped = true;
            self.display_current = false;
            return Err(());
        }
        self.drops = 0;
        slot.clear();
        self.shared = None;
        Ok(None)
    }

    fn write_patches(&mut self, out: &mut Vec<u8>, width: u32) {
        for index in 0..self.patch_count {
            delete_image(
                out,
                FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX),
            );
        }
        for (index, rect) in self.rects.iter().enumerate() {
            let id = FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX);
            extract(&self.image, width, *rect, &mut self.patch_pixels);
            move_cursor(out, rect.x, rect.y, self.cell_size);
            image(
                out,
                &mut self.encoder,
                id,
                rect.width,
                rect.height,
                &self.patch_pixels,
                true,
            );
        }
        self.drops = 0;
        self.patch_count = self.rects.len();
    }

    fn write_whole(
        &mut self,
        out: &mut Vec<u8>,
        width: u32,
        height: u32,
        file: Option<File>,
        prepared: Option<bool>,
    ) {
        // Delete image data and clear placements before replacement.
        delete_all(out);
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        let shared = self
            .shared
            .as_ref()
            .zip(file)
            .is_some_and(|(slot, file)| slot.transfer(out, file, width, height, &self.image));
        if !shared {
            if let Some(compressed) = prepared {
                write_image(
                    out,
                    &self.encoder,
                    SCREEN_ID,
                    (width, height),
                    &self.image,
                    false,
                    compressed,
                );
            } else {
                image(
                    out,
                    &mut self.encoder,
                    SCREEN_ID,
                    width,
                    height,
                    &self.image,
                    false,
                );
            }
        }
        self.patch_count = 0;
        // Retain a base only for patches or recovery from dropped shared
        // frames.
        self.base = (self.cell_size.is_some() || self.shared.is_some()).then(|| Frame {
            width,
            height,
            pixels: Arc::clone(&self.image),
        });
        self.damage = 0..0;
    }

    fn apply_band(&mut self, width: u32, height: u32, y: u32, band: Vec<u8>) -> Option<bool> {
        let (Ok(stride), Ok(rows), Ok(top)) = (
            usize::try_from(width),
            usize::try_from(height),
            usize::try_from(y),
        ) else {
            return None;
        };
        let stride = stride.checked_mul(3)?;
        if width == 0 || height == 0 || band.is_empty() || !band.len().is_multiple_of(stride) {
            return None;
        }
        let start = top.checked_mul(stride)?;
        let end = start.checked_add(band.len())?;
        if end > rows.checked_mul(stride)? {
            return None;
        }

        // Preserve the allocation for identical full frames, including a shared
        // base.
        if start == 0 && end == rows * stride {
            if self.width == width
                && self.height == height
                && self.image.as_slice() == band.as_slice()
            {
                return Some(false);
            }
            self.width = width;
            self.height = height;
            self.image = Arc::new(band);
            self.damage = 0..height;
            return Some(true);
        }

        // A resized partial frame starts empty; `base` still describes the
        // terminal's old image for comparison.
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.image = Arc::new(vec![0; rows * stride]);
            self.damage = 0..height;
        } else if &self.image[start..end] == band.as_slice() {
            return Some(false);
        }
        Arc::make_mut(&mut self.image)[start..end].copy_from_slice(&band);
        let bottom = (end / stride) as u32;
        self.damage = if self.damage.is_empty() {
            y..bottom
        } else {
            self.damage.start.min(y)..self.damage.end.max(bottom)
        };
        Some(true)
    }

    /// Compare cell-aligned patches against the last whole image. A shared
    /// slot only pays for a diff small enough that the whole-frame transfer it
    /// replaces would be the larger one.
    fn select_patches(&mut self, width: u32, height: u32) -> bool {
        self.rects.clear();
        let Some(old) = self
            .base
            .as_ref()
            .filter(|old| old.width == width && old.height == height)
        else {
            return false;
        };
        let Some((cell_w, cell_h)) = self.cell_size else {
            return false;
        };
        if cell_w == 0 || cell_h == 0 {
            return false;
        }
        let area = u64::from(width) * u64::from(height);
        let limit = if self.shared.is_some() {
            area / SHARED_PATCH_DIVISOR
        } else {
            area
        };
        let patches = changed_rects(
            old,
            &self.image,
            u32::from(cell_w),
            u32::from(cell_h),
            limit,
            self.damage.clone(),
            &mut self.rects,
        );
        if patches {
            self.damage = self.rects.iter().fold(0..0, |rows, rect| {
                if rows.is_empty() {
                    rect.y..rect.y + rect.height
                } else {
                    rows.start.min(rect.y)..rows.end.max(rect.y + rect.height)
                }
            });
        }
        patches
    }
}

/// Build cell-aligned changes; more than `limit` covered pixels or more than
/// [`MAX_PATCHES`] patches goes whole.
fn changed_rects(
    old: &Frame,
    new: &[u8],
    cell_w: u32,
    cell_h: u32,
    limit: u64,
    damage: Range<u32>,
    rects: &mut Vec<Rect>,
) -> bool {
    let cols = old.width.div_ceil(cell_w);
    let rows = damage.start / cell_h..damage.end.div_ceil(cell_h);
    let mut covered = 0u64;
    for row in rows {
        let y = row * cell_h;
        let h = cell_h.min(old.height - y);
        let row_start = y as usize * old.width as usize * 3;
        let row_end = (y + h) as usize * old.width as usize * 3;
        if old.pixels[row_start..row_end] == new[row_start..row_end] {
            continue;
        }
        let mut col = 0;
        while col < cols {
            let x = col * cell_w;
            let w = cell_w.min(old.width - x);
            if !different(old, new, x, y, w, h) {
                col += 1;
                continue;
            }
            let start = col;
            col += 1;
            while col < cols {
                let next_x = col * cell_w;
                let next_w = cell_w.min(old.width - next_x);
                if !different(old, new, next_x, y, next_w, h) {
                    break;
                }
                col += 1;
            }
            let rect = Rect {
                x: start * cell_w,
                y,
                width: ((col - start) * cell_w).min(old.width - start * cell_w),
                height: h,
            };
            if !rect.width.is_multiple_of(cell_w) || !rect.height.is_multiple_of(cell_h) {
                return false;
            }
            covered += u64::from(rect.width) * u64::from(rect.height);
            if covered >= limit {
                return false;
            }
            // Merge vertically adjacent equal-width runs.
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
                return false;
            }
        }
    }
    true
}

fn different(old: &Frame, new: &[u8], x: u32, y: u32, width: u32, height: u32) -> bool {
    let stride = old.width as usize * 3;
    for row in y as usize..(y + height) as usize {
        let start = x as usize * 3;
        let end = (x + width) as usize * 3;
        if old.pixels[row * stride + start..row * stride + end]
            != new[row * stride + start..row * stride + end]
        {
            return true;
        }
    }
    false
}

fn extract(image: &[u8], width: u32, rect: Rect, out: &mut Vec<u8>) {
    let stride = width as usize * 3;
    let row_len = rect.width as usize * 3;
    out.clear();
    out.reserve(row_len * rect.height as usize);
    for row in rect.y as usize..(rect.y + rect.height) as usize {
        let start = rect.x as usize * 3;
        out.extend_from_slice(&image[row * stride + start..row * stride + start + row_len]);
    }
}

fn move_cursor(out: &mut Vec<u8>, x: u32, y: u32, cell_size: Option<(u16, u16)>) {
    // Zero cell dimensions must not divide cursor coordinates.
    let (cell_w, cell_h) =
        cell_size.map_or((1, 1), |(w, h)| (u32::from(w).max(1), u32::from(h).max(1)));
    let col = x / cell_w + 1;
    let row = y / cell_h + 1;
    write!(out, "\x1b[{row};{col}H").expect("writing to Vec cannot fail");
}

fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").expect("writing to Vec cannot fail");
}

/// Common kitty transmission keys; callers append transport and chunk keys.
fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, placement: u32) {
    write!(
        out,
        "a=T,f=24,s={width},v={height},i={id},p={placement},z=1,C=1,q=2"
    )
    .expect("writing to Vec cannot fail");
}

fn image(
    out: &mut Vec<u8>,
    encoder: &mut EncodingBuffers,
    id: u32,
    width: u32,
    height: u32,
    pixels: &[u8],
    patch: bool,
) {
    let compressed = prepare_image(encoder, pixels);
    write_image(out, encoder, id, (width, height), pixels, patch, compressed);
}

fn prepare_image(encoder: &mut EncodingBuffers, pixels: &[u8]) -> bool {
    compress(&mut encoder.compressed, &mut encoder.sample, pixels)
        && encoder.compressed.len() * 4 <= pixels.len() * 3
}

fn encoded_capacity(payload_len: usize) -> usize {
    // Budget at most 16 bytes per continuation and 96 for the first command.
    let chunks = payload_len.div_ceil(CHUNK_PAYLOAD).max(1);
    payload_len.div_ceil(3) * 4 + chunks * 16 + 96
}

fn write_image(
    out: &mut Vec<u8>,
    encoder: &EncodingBuffers,
    id: u32,
    size: (u32, u32),
    pixels: &[u8],
    patch: bool,
    zlib: bool,
) {
    let (width, height) = size;
    let payload: &[u8] = if zlib { &encoder.compressed } else { pixels };
    let compression = if zlib {
        b",o=z".as_slice()
    } else {
        b"".as_slice()
    };
    out.reserve(encoded_capacity(payload.len()));
    // Encode directly into output chunks rather than allocating a whole
    // base64 payload and copying it.
    let mut encoded_chunk = [0u8; CHUNK];
    let mut chunks = payload.chunks(CHUNK_PAYLOAD).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let bytes = STANDARD
            .encode_slice(chunk, &mut encoded_chunk)
            .expect("a chunk of payload always fits its base64");
        // Only continuations use `m=0`; earlier chunks use `m=1`.
        let more = chunks.peek().is_some();
        if first {
            out.extend_from_slice(b"\x1b_G");
            transmit(out, id, width, height, u32::from(patch));
            out.extend_from_slice(compression);
            if more {
                out.extend_from_slice(b",m=1");
            }
            out.push(b';');
            first = false;
        } else if more {
            out.extend_from_slice(b"\x1b_Gm=1;");
        } else {
            out.extend_from_slice(b"\x1b_Gm=0;");
        }
        out.extend_from_slice(&encoded_chunk[..bytes]);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Sample large frames before spending a full compression pass on noise.
fn compress(compressed: &mut Vec<u8>, sample: &mut Vec<u8>, data: &[u8]) -> bool {
    const SAMPLE_BYTES: usize = 16 * 1024;
    const SAMPLES: usize = 4;
    if data.len() > SAMPLE_BYTES * SAMPLES {
        sample.clear();
        sample.reserve(SAMPLE_BYTES * SAMPLES);
        for index in 0..SAMPLES {
            let start = (data.len() - SAMPLE_BYTES) * index / (SAMPLES - 1);
            sample.extend_from_slice(&data[start..start + SAMPLE_BYTES]);
        }
        if !zlib(compressed, sample) || compressed.len() * 4 > sample.len() * 3 {
            return false;
        }
    }
    zlib(compressed, data)
}

/// Compress into reusable output storage.
fn zlib(out: &mut Vec<u8>, data: &[u8]) -> bool {
    out.clear();
    let mut encoder = ZlibEncoder::new(&mut *out, Compression::fast());
    encoder.write_all(data).is_ok() && encoder.finish().is_ok()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::Read};

    fn whole(presenter: &mut Presenter, width: u32, height: u32, frame: &[u8]) -> Vec<u8> {
        presenter.present(width, height, 0, frame.to_vec())
    }

    use super::*;

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

    /// Decode emitted kitty commands into a simulated image layer.
    struct Replay {
        width: usize,
        height: usize,
        cell: (usize, usize),
        cursor: (usize, usize),
        images: HashMap<u32, Vec<u8>>,
        placements: HashMap<(u32, u32), Placement>,
        /// Simulates a terminal that has not consumed the shared object.
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

    /// Simulate the terminal unlinking a shared object after reading it.
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
        // No terminal will unlink the test probe object.
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
        let first = whole(&mut presenter, 4, 2, &base);
        assert_eq!(whole_frames(&first).len(), 1);
        assert_eq!(patches(&first), Vec::<String>::new());

        let mut changed = base;
        put_block(&mut changed, 4, 0, 0, 2, 2, 0xab);
        let second = whole(&mut presenter, 4, 2, &changed);
        assert_eq!(patches(&second).len(), 1);
        assert_eq!(whole_frames(&second).len(), 0);
        assert_eq!(replay(&[first, second], 4, 2, (2, 2)), changed);
    }

    #[test]
    fn unchanged_frame_is_empty() {
        let mut presenter = Presenter::new(None, None);
        let rgb = vec![1; 12];
        assert_ne!(whole(&mut presenter, 2, 2, &rgb), Vec::<u8>::new());
        assert_eq!(whole(&mut presenter, 2, 2, &rgb), Vec::<u8>::new());
    }

    #[test]
    fn whole_frame_transmit_has_exact_parameters() {
        let mut presenter = Presenter::new(None, None);
        let update = whole(&mut presenter, 2, 2, &noise(&mut 7, 2 * 2 * 3));
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
        let first = whole(&mut presenter, 4, 4, &base);
        let mut changed = base;
        let cell = noise(&mut 3, 2 * 2 * 3);
        for row in 0..2 {
            let start = ((2 + row) * 4 + 2) * 3;
            changed[start..start + 6].copy_from_slice(&cell[row * 6..row * 6 + 6]);
        }
        let second = whole(&mut presenter, 4, 4, &changed);

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
        let first = whole(&mut presenter, 4, 4, &base);
        let mut changed = base;
        put_block(&mut changed, 4, 0, 0, 2, 2, 200);
        let second = whole(&mut presenter, 4, 4, &changed);
        assert_eq!(patches(&second).len(), 1);

        let mut larger = vec![0; 6 * 4 * 3];
        put_block(&mut larger, 6, 4, 0, 2, 2, 90);
        let third = whole(&mut presenter, 6, 4, &larger);
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
        let first = whole(&mut presenter, 4, 4, &base);
        let mut changed = base.clone();
        put_block(&mut changed, 4, 2, 2, 2, 2, 255);
        let second = whole(&mut presenter, 4, 4, &changed);
        assert_eq!(patches(&second).len(), 1);

        let third = whole(&mut presenter, 4, 4, &base);
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
        let first = whole(&mut presenter, 4, 4, &base);
        let mut two = base.clone();
        put_block(&mut two, 4, 0, 0, 2, 2, 30);
        put_block(&mut two, 4, 2, 2, 2, 2, 60);
        let second = whole(&mut presenter, 4, 4, &two);
        assert_eq!(patches(&second).len(), 2);

        let mut one = base;
        put_block(&mut one, 4, 0, 0, 2, 2, 30);
        let third = whole(&mut presenter, 4, 4, &one);
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
        // Alternate changed and untouched cells to exceed the patch limit.
        let width = runs * 4 + 2;
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let base = vec![0; width as usize * 2 * 3];
        let first = whole(&mut presenter, width, 2, &base);
        let mut scattered = base;
        for run in 0..runs {
            put_block(&mut scattered, width, run * 4, 0, 2, 2, 100);
        }
        let second = whole(&mut presenter, width, 2, &scattered);
        assert!(patches(&second).is_empty(), "{runs} runs exceed the limit");
        assert_eq!(
            replay(&[first, second], width as usize, 2, (2, 2)),
            scattered
        );

        let mut coverage = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 4 * 4 * 3];
        let first = whole(&mut coverage, 4, 4, &base);
        let full = vec![200; 4 * 4 * 3];
        let second = whole(&mut coverage, 4, 4, &full);
        assert!(patches(&second).is_empty(), "a full frame is not a patch");
        assert_eq!(replay(&[first, second], 4, 4, (2, 2)), full);

        let mut unaligned = Presenter::new(Some((2, 2)), None);
        let base = vec![0; 5 * 2 * 3];
        let first = whole(&mut unaligned, 5, 2, &base);
        let mut ragged = base;
        put_block(&mut ragged, 5, 4, 0, 1, 2, 77);
        let second = whole(&mut unaligned, 5, 2, &ragged);
        assert!(
            patches(&second).is_empty(),
            "a partial cell cannot be patched"
        );
        assert_eq!(replay(&[first, second], 5, 2, (2, 2)), ragged);
    }

    /// A slot only pays for diffs small enough that the whole transfer it
    /// replaces would be the larger one.
    #[test]
    fn a_shared_slot_turns_a_large_diff_into_a_whole_frame() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let base = vec![0; 8 * 8 * 3];
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let mut replay = Replay::new(8, 8, (2, 2));
        replay.feed(&whole(&mut presenter, 8, 8, &base));

        // One cell, 4 of 64 pixels: a patch.
        let mut small = base.clone();
        small[..6].fill(0xff);
        let update = whole(&mut presenter, 8, 8, &small);
        assert_eq!(patches(&update).len(), 1, "a small diff stays a patch");
        assert_eq!(whole_frames(&update).len(), 0);
        replay.feed(&update);
        assert_eq!(replay.screen(), small);

        // Half the frame, 32 of 64 pixels: one transfer through the slot.
        let mut large = base.clone();
        for row in 0..4 {
            let start = row * 8 * 3;
            large[start..start + 8 * 3].fill(0xff);
        }
        let update = whole(&mut presenter, 8, 8, &large);
        let control = whole_frames(&update);
        assert_eq!(control.len(), 1, "a large diff goes whole");
        assert!(control[0].contains("t=s"), "through the slot: {control:?}");
        replay.feed(&update);
        assert_eq!(replay.screen(), large);

        // The same diff without a slot is cheaper as one patch.
        let mut pty = Presenter::new(Some((2, 2)), None);
        let _ = whole(&mut pty, 8, 8, &base);
        let update = whole(&mut pty, 8, 8, &large);
        assert_eq!(patches(&update).len(), 1, "without a slot it stays patches");
        assert_eq!(whole_frames(&update).len(), 0);
    }

    #[test]
    fn payload_chunks_respect_the_protocol_framing() {
        let mut presenter = Presenter::new(None, None);
        let update = whole(&mut presenter, 100, 100, &noise(&mut 5, 30_000));

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
            let update = whole(&mut presenter, WIDTH as u32, HEIGHT as u32, &frame);
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
        let update = whole(&mut presenter, 4, 2, &frame);
        let control = whole_frames(&update);
        assert_eq!(control.len(), 1);
        assert!(
            control[0].contains("t=s"),
            "not a shared transfer: {control:?}"
        );
        assert_eq!(replay(&[update], 4, 2, (2, 2)), frame);
    }

    /// An unread shared object drops the next frame until the terminal catches
    /// up.
    #[test]
    fn a_slot_the_terminal_has_not_read_drops_the_frame() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let mut replay = Replay::new(4, 4, (2, 2));
        replay.read_shared = false;
        let first = whole(&mut presenter, 4, 2, &noise(&mut 13, 4 * 2 * 3));
        replay.feed(&first);

        let second = noise(&mut 17, 4 * 4 * 3);
        let update = whole(&mut presenter, 4, 4, &second);
        assert!(update.is_empty(), "a dropped frame writes nothing");
        assert!(presenter.dropped(), "the pane has to say it drew nothing");
        assert_eq!(whole_frames(&update), Vec::<String>::new());

        replay.read_shared = true;
        replay.feed(&first);
        let update = whole(&mut presenter, 4, 4, &second);
        assert!(!presenter.dropped());
        assert!(
            whole_frames(&update)[0].contains("t=s"),
            "not shared memory"
        );
        replay.feed(&update);
        assert_eq!(replay.screen(), second);
    }

    #[test]
    fn bands_compose_into_the_frame_the_terminal_shows() {
        const WIDTH: u32 = 4;
        const HEIGHT: u32 = 4;
        let mut presenter = Presenter::new(Some((2, 2)), None);
        let mut frame = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
        let first = whole(&mut presenter, WIDTH, HEIGHT, &frame);
        assert_eq!(whole_frames(&first).len(), 1);

        let mut replay = Replay::new(WIDTH as usize, HEIGHT as usize, (2, 2));
        replay.feed(&first);

        for row in 2..HEIGHT {
            for column in 0..WIDTH {
                let at = ((row * WIDTH + column) * 3) as usize;
                frame[at..at + 3].copy_from_slice(&[7, 8, 9]);
            }
        }
        let band = frame[(2 * WIDTH * 3) as usize..].to_vec();
        let update = presenter.present(WIDTH, HEIGHT, 2, band.clone());
        assert_eq!(patches(&update).len(), 1, "one cell row, two cells wide");
        replay.feed(&update);
        assert_eq!(replay.screen(), frame, "the composed frame is on screen");

        let again = presenter.present(WIDTH, HEIGHT, 2, band);
        assert_eq!(again.len(), 0, "an unchanged band needs no transfer");
        replay.feed(&again);
        assert_eq!(replay.screen(), frame);
    }

    #[test]
    fn an_unchanged_band_still_redraws_after_the_grid_changes() {
        let mut presenter = Presenter::new(None, None);
        let frame = vec![17; 4 * 4 * 3];
        let first = whole(&mut presenter, 4, 4, &frame);
        let band = frame[2 * 4 * 3..].to_vec();
        assert_eq!(
            presenter.present(4, 4, 2, band.clone()).len(),
            0,
            "an unchanged band needs no transfer"
        );

        presenter.set_cell_size(Some((2, 2)));
        let redraw = presenter.present(4, 4, 2, band);
        assert_eq!(whole_frames(&redraw).len(), 1);
        assert_eq!(replay(&[first, redraw], 4, 4, (2, 2)), frame);
    }

    #[test]
    fn a_partial_band_without_a_patch_grid_replaces_the_whole_image() {
        let mut presenter = Presenter::new(None, None);
        let mut frame = vec![17; 4 * 4 * 3];
        let first = whole(&mut presenter, 4, 4, &frame);
        let band = vec![31; 2 * 4 * 3];
        frame[2 * 4 * 3..].copy_from_slice(&band);
        let next = presenter.present(4, 4, 2, band);
        assert_eq!(whole_frames(&next).len(), 1);
        assert_eq!(replay(&[first, next], 4, 4, (1, 1)), frame);
    }

    #[test]
    fn unchanged_band_retries_a_dropped_shared_frame() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(None, Some(shared));
        let base = vec![0; 4 * 4 * 3];
        let first = whole(&mut presenter, 4, 4, &base);
        let band = vec![93; 2 * 4 * 3];
        assert_eq!(
            presenter.present(4, 4, 2, band.clone()).len(),
            0,
            "an unchanged band needs no transfer"
        );
        assert!(presenter.dropped());

        // An unchanged band still needs transfer after a dropped attempt.
        let mut terminal = Replay::new(4, 4, (1, 1));
        terminal.feed(&first);
        let update = presenter.present(4, 4, 2, band.clone());
        assert!(!presenter.dropped());
        assert!(whole_frames(&update)[0].contains("t=s"));
        terminal.feed(&update);
        let mut expected = base;
        expected[2 * 4 * 3..].copy_from_slice(&band);
        assert_eq!(terminal.screen(), expected);
    }

    /// Recovering the displayed frame after a drop must not report another
    /// drop.
    #[test]
    fn an_unchanged_frame_after_a_drop_is_not_reported_dropped() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let first = noise(&mut 3, 4 * 2 * 3);
        assert_ne!(whole(&mut presenter, 4, 2, &first), Vec::<u8>::new());
        assert!(!presenter.dropped(), "the first frame is drawn");

        let second = noise(&mut 5, 4 * 4 * 3);
        assert_eq!(whole(&mut presenter, 4, 4, &second), Vec::<u8>::new());
        assert!(presenter.dropped());

        // The old frame is already displayed; no transfer is needed.
        assert_eq!(whole(&mut presenter, 4, 2, &first), Vec::<u8>::new());
        assert!(!presenter.dropped());
    }

    /// Permanently fall back to pty frames if shared objects stay unread.
    #[test]
    fn a_slot_that_is_never_read_falls_back_to_the_pty() {
        let Some(shared) = shared_memory() else {
            return;
        };
        let mut presenter = Presenter::new(Some((2, 2)), Some(shared));
        let mut replay = Replay::new(4, 4, (2, 2));
        replay.read_shared = false;
        let mut frame = vec![0; 4 * 4 * 3];
        let first = whole(&mut presenter, 4, 4, &frame);
        replay.feed(&first);
        assert!(!presenter.dropped());

        for step in 1..DROP_LIMIT {
            // Every cell differs, ruling out patches.
            frame.fill(step as u8);
            let update = whole(&mut presenter, 4, 4, &frame);
            assert!(update.is_empty(), "step {step} wrote something");
            assert!(
                presenter.dropped(),
                "step {step} did not say it was dropped"
            );
        }

        frame.fill(0xab);
        let update = whole(&mut presenter, 4, 4, &frame);
        assert!(!update.is_empty(), "the fallback frame must be drawn");
        assert!(!presenter.dropped());
        assert!(
            whole_frames(&update)
                .iter()
                .all(|control| !control.contains("t=s")),
            "the fallback is a pty frame: {update:?}"
        );
        replay.feed(&update);
        assert_eq!(replay.screen(), frame);

        // Fallback must not restart the shared-memory drop cycle.
        frame.fill(0xcd);
        let next = whole(&mut presenter, 4, 4, &frame);
        assert!(!presenter.dropped());
        assert!(
            whole_frames(&next)
                .iter()
                .all(|control| !control.contains("t=s"))
        );
        replay.feed(&next);
        assert_eq!(replay.screen(), frame);
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
