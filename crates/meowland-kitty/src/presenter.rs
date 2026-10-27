//! Encode RGB frames for the kitty graphics protocol.

use std::{
    fs::File,
    io::{self, Write},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use meowland_core::{pixels::FrameSize, timing::FRAME_INTERVAL};
use meowland_protocol::{FrameBand, SharedFrame as PublishedFrame};
use rustix::{fs::Mode, shm};

use crate::encoding::{Encoder, Encoding};

const MAX_PATCHES: usize = 32;
const SHARED_TIMEOUT: Duration = Duration::from_millis(250);
/// Most of a frame a cell-aligned diff may cover while a shared slot is
/// available: past this, one whole transfer into the object costs less than
/// megabytes of patches on the pty.
const SHARED_PATCH_DIVISOR: u64 = 4;
const SCREEN_ID: u32 = 1;
const FIRST_PATCH_ID: u32 = 2;
/// Image ID reserved for the shared-memory capability probe.
pub const PROBE_ID: u32 = 32;

/// POSIX shared-memory slot for whole frames. A terminal unlinks each object
/// after reading it; persistent unread objects disable this path.
#[derive(Debug)]
pub struct SharedMemory {
    name: String,
    encoded_name: String,
}

impl SharedMemory {
    /// Reserve a process-unique shared-memory name without opening an object
    /// yet.
    #[must_use]
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "/meowland-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let encoded_name = STANDARD.encode(name.as_bytes());
        let slot = Self { name, encoded_name };
        slot.clear();
        slot
    }

    /// Query shared-memory support with a one-pixel object, if creation
    /// succeeds.
    #[must_use]
    pub fn probe(&self) -> Option<Vec<u8>> {
        let mut file = self.create().ok()?;
        if file.write_all(&[0; 3]).is_err() {
            self.clear();
            return None;
        }
        Some(
            format!(
                "\x1b_Ga=q,f=24,s=1,v=1,i={PROBE_ID},t=s;{}\x1b\\",
                self.encoded_name
            )
            .into_bytes(),
        )
    }

    /// Best-effort unlink of an unread shared object; an already-unlinked
    /// object is harmless.
    pub fn clear(&self) {
        let _ = shm::unlink(self.name.as_str());
    }

    fn create(&self) -> io::Result<File> {
        shm::open(
            self.name.as_str(),
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(io::Error::from)
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
        out.reserve(self.encoded_name.len() + 96);
        out.extend_from_slice(b"\x1b_G");
        transmit(out, SCREEN_ID, width, height, Placement::Whole);
        out.extend_from_slice(b",t=s;");
        out.extend_from_slice(self.encoded_name.as_bytes());
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
    #[expect(
        clippy::rc_buffer,
        reason = "Vec preserves ownership transfer of incoming frame allocations and copy-on-write reuse"
    )]
    image: Arc<Vec<u8>>,
    size: Option<FrameSize>,
    /// Base under patches, or a shared frame awaiting confirmation.
    base: Option<Frame>,
    patch_count: usize,
    /// Whether `image` is already displayed.
    display_current: bool,
    rects: Vec<Rect>,
    /// Rows that may differ from the whole-image base, including live patches.
    damage: Range<u32>,
    shared: Option<SharedMemory>,
    encoder: Encoder,
    patch_pixels: Vec<u8>,
    shared_blocked_since: Option<Instant>,
}

#[derive(Debug)]
struct Frame {
    size: FrameSize,
    #[expect(
        clippy::rc_buffer,
        reason = "the base shares the composed frame allocation without converting or copying its Vec"
    )]
    pixels: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
enum SharedFrame {
    Inline,
    Ready(File),
    Unread(Instant),
}

/// Whether a retained image was encoded, unchanged, or temporarily deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    /// Retained pixels already match the displayed image.
    Unchanged,
    /// Terminal commands were appended to the output buffer.
    Encoded,
    /// An unread shared frame prevents progress until this retry deadline.
    RetryAt(Instant),
    /// A partial update arrived without a matching full-frame basis.
    NeedsKeyframe,
}

impl Presenter {
    /// Enable patches with known cell dimensions and shared frames with a
    /// terminal-confirmed shared-memory slot.
    #[must_use]
    pub fn new(cell_size: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            cell_size,
            shared,
            ..Self::default()
        }
    }

    /// Changing the grid invalidates displayed patches.
    pub fn set_cell_size(&mut self, cell_size: Option<(u16, u16)>) -> bool {
        if self.cell_size == cell_size {
            return false;
        }
        self.cell_size = cell_size;
        self.invalidate();
        true
    }

    /// Forget displayed placements without discarding retained pixel storage.
    pub fn invalidate(&mut self) {
        self.base = None;
        self.patch_count = 0;
        self.display_current = false;
    }

    /// Retain a validated row band and encode its terminal update.
    pub fn present_into(&mut self, band: FrameBand, out: &mut Vec<u8>) -> PresentOutcome {
        out.clear();
        let Some(changed) = self.apply_band(band) else {
            return PresentOutcome::NeedsKeyframe;
        };
        if changed {
            self.display_current = false;
        }
        self.repaint_into(out)
    }

    /// Adopt a complete frame already displayed through an upstream transfer.
    ///
    /// Returns false for partial bands, leaving the presenter unchanged. This
    /// restores the pixel basis lazily before patches or a grid-change repaint.
    pub fn retain_frame(&mut self, band: FrameBand) -> bool {
        if band.y() != 0 || band.rgb().len() != band.size().rgb_len() {
            return false;
        }
        let (size, _, pixels) = band.into_parts();
        self.image = Arc::new(pixels);
        self.size = Some(size);
        self.base = Some(Frame {
            size,
            pixels: Arc::clone(&self.image),
        });
        self.patch_count = 0;
        self.display_current = true;
        self.damage = 0..0;
        true
    }

    /// Retry or invalidate terminal presentation without requesting pixels
    /// again.
    ///
    /// # Panics
    /// Panics if internal damage rows exceed the validated frame dimensions.
    pub fn repaint_into(&mut self, out: &mut Vec<u8>) -> PresentOutcome {
        out.clear();
        let Some(size) = self.size else {
            return PresentOutcome::Unchanged;
        };
        if self.display_current {
            return PresentOutcome::Unchanged;
        }
        let (width, height) = (size.width(), size.height());

        if self.patch_count == 0
            && self.base.as_ref().is_some_and(|old| {
                Some(old.size) == self.size
                    && (Arc::ptr_eq(&old.pixels, &self.image) || {
                        let stride = size.rgb_stride();
                        let start = usize::try_from(self.damage.start)
                            .expect("damage starts within validated frame height");
                        let end = usize::try_from(self.damage.end)
                            .expect("damage ends within validated frame height");
                        let rows = start * stride..end * stride;
                        old.pixels[rows.clone()] == self.image[rows]
                    })
            })
        {
            self.display_current = true;
            return PresentOutcome::Unchanged;
        }

        let patches = self.select_patches();

        let file = if patches {
            None
        } else {
            match self.ready_shared() {
                SharedFrame::Inline => None,
                SharedFrame::Ready(file) => Some(file),
                SharedFrame::Unread(deadline) => return PresentOutcome::RetryAt(deadline),
            }
        };

        let prepared = (!patches && file.is_none()).then(|| self.encoder.prepare(&self.image));
        let capacity = prepared.map_or(64, |encoding| {
            self.encoder.capacity(&self.image, encoding) + 64
        });
        out.reserve(capacity);
        out.extend_from_slice(b"\x1b[?2026h");

        if patches {
            self.write_patches(out, width);
        } else {
            self.write_whole(out, width, height, file, prepared);
        }

        out.extend_from_slice(b"\x1b[?2026l");
        self.display_current = true;
        PresentOutcome::Encoded
    }

    /// Retry unread shared frames on a clock, then fall back to inline
    /// transport.
    fn ready_shared(&mut self) -> SharedFrame {
        let Some(slot) = &self.shared else {
            return SharedFrame::Inline;
        };
        match slot.create() {
            Ok(file) => {
                self.shared_blocked_since = None;
                return SharedFrame::Ready(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                tracing::warn!(%error, "Shared-memory transport unavailable; using inline frames");
                self.shared = None;
                self.shared_blocked_since = None;
                return SharedFrame::Inline;
            }
        }
        let now = Instant::now();
        let blocked = *self.shared_blocked_since.get_or_insert(now);
        if now.duration_since(blocked) < SHARED_TIMEOUT {
            return SharedFrame::Unread((now + FRAME_INTERVAL).min(blocked + SHARED_TIMEOUT));
        }
        tracing::warn!("Shared-memory image remained unread; using inline transport");
        self.shared_blocked_since = None;
        slot.clear();
        self.shared = None;
        SharedFrame::Inline
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
            rect.extract(&self.image, width, &mut self.patch_pixels);
            move_cursor(out, rect.x, rect.y, self.cell_size);
            self.encoder.image(
                out,
                id,
                (rect.width, rect.height),
                &self.patch_pixels,
                Placement::Patch,
            );
        }
        self.patch_count = self.rects.len();
    }

    fn write_whole(
        &mut self,
        out: &mut Vec<u8>,
        width: u32,
        height: u32,
        file: Option<File>,
        prepared: Option<Encoding>,
    ) {
        delete_all(out);
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        let shared = self
            .shared
            .as_ref()
            .zip(file)
            .is_some_and(|(slot, file)| slot.transfer(out, file, width, height, &self.image));
        if !shared {
            self.shared = None;
            if let Some(encoding) = prepared {
                self.encoder.write(
                    out,
                    SCREEN_ID,
                    (width, height),
                    &self.image,
                    Placement::Whole,
                    encoding,
                );
            } else {
                self.encoder.image(
                    out,
                    SCREEN_ID,
                    (width, height),
                    &self.image,
                    Placement::Whole,
                );
            }
        }
        self.patch_count = 0;
        self.base = self
            .size
            .filter(|_| self.cell_size.is_some() || self.shared.is_some())
            .map(|size| Frame {
                size,
                pixels: Arc::clone(&self.image),
            });
        self.damage = 0..0;
    }

    fn apply_band(&mut self, band: FrameBand) -> Option<bool> {
        let (size, y, band) = band.into_parts();
        let bytes = size.rgb_band(y, band.len())?;
        let (start, end) = (bytes.start, bytes.end);
        let stride = size.rgb_stride();
        let height = size.height();
        let rows = usize::try_from(height).expect("validated frame height fits usize");

        if start == 0 && end == rows * stride {
            if self.size == Some(size) && self.image.as_slice() == band.as_slice() {
                return Some(false);
            }
            self.size = Some(size);
            self.image = Arc::new(band);
            self.damage = 0..height;
            return Some(true);
        }

        if self.size != Some(size) {
            return None;
        }
        if &self.image[start..end] == band.as_slice() {
            return Some(false);
        }
        Arc::make_mut(&mut self.image)[start..end].copy_from_slice(&band);
        let bottom = u32::try_from(end / stride).expect("band ends within validated frame height");
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
    fn select_patches(&mut self) -> bool {
        self.rects.clear();
        let Some(old) = self.base.as_ref().filter(|old| Some(old.size) == self.size) else {
            return false;
        };
        let Some((cell_w, cell_h)) = self.cell_size else {
            return false;
        };
        if cell_w == 0 || cell_h == 0 {
            return false;
        }
        let area = u64::from(old.size.width()) * u64::from(old.size.height());
        let limit = if self.shared.is_some() {
            area / SHARED_PATCH_DIVISOR
        } else {
            area
        };
        let patches = old.changed_rects(
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

impl Frame {
    /// Build cell-aligned changes; more than `limit` covered pixels or more
    /// than [`MAX_PATCHES`] patches goes whole.
    fn changed_rects(
        &self,
        new: &[u8],
        cell_w: u32,
        cell_h: u32,
        limit: u64,
        damage: Range<u32>,
        rects: &mut Vec<Rect>,
    ) -> bool {
        let (width, height) = (self.size.width(), self.size.height());
        let cols = width.div_ceil(cell_w);
        let rows = damage.start / cell_h..damage.end.div_ceil(cell_h);
        let mut covered = 0u64;
        for row in rows {
            let y = row * cell_h;
            let h = cell_h.min(height - y);
            let stride = self.size.rgb_stride();
            let row_start =
                usize::try_from(y).expect("patch starts within validated height") * stride;
            let row_end =
                usize::try_from(y + h).expect("patch ends within validated height") * stride;
            if self.pixels[row_start..row_end] == new[row_start..row_end] {
                continue;
            }
            let mut col = 0;
            while col < cols {
                let x = col * cell_w;
                let w = cell_w.min(width - x);
                if !self.different(new, x, y, w, h) {
                    col += 1;
                    continue;
                }
                let start = col;
                col += 1;
                while col < cols {
                    let next_x = col * cell_w;
                    let next_w = cell_w.min(width - next_x);
                    if !self.different(new, next_x, y, next_w, h) {
                        break;
                    }
                    col += 1;
                }
                let rect = Rect {
                    x: start * cell_w,
                    y,
                    width: ((col - start) * cell_w).min(width - start * cell_w),
                    height: h,
                };
                if !rect.width.is_multiple_of(cell_w) || !rect.height.is_multiple_of(cell_h) {
                    return false;
                }
                covered += u64::from(rect.width) * u64::from(rect.height);
                if covered >= limit {
                    return false;
                }
                if let Some(previous) = rects.iter_mut().rev().find(|previous| {
                    previous.x == rect.x
                        && previous.width == rect.width
                        && previous.y + previous.height == rect.y
                }) {
                    previous.height += rect.height;
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

    fn different(&self, new: &[u8], x: u32, y: u32, width: u32, height: u32) -> bool {
        let stride = self.size.rgb_stride();
        let top = usize::try_from(y).expect("patch starts within validated height");
        let bottom = usize::try_from(y + height).expect("patch ends within validated height");
        let start = usize::try_from(x).expect("patch starts within validated width") * 3;
        let end = usize::try_from(x + width).expect("patch ends within validated width") * 3;
        for row in top..bottom {
            if self.pixels[row * stride + start..row * stride + end]
                != new[row * stride + start..row * stride + end]
            {
                return true;
            }
        }
        false
    }
}

impl Rect {
    fn extract(self, image: &[u8], width: u32, out: &mut Vec<u8>) {
        let stride = usize::try_from(width).expect("validated frame width fits usize") * 3;
        let row_len = usize::try_from(self.width).expect("patch width fits the frame") * 3;
        let height = usize::try_from(self.height).expect("patch height fits the frame");
        let top = usize::try_from(self.y).expect("patch starts within validated height");
        let start = usize::try_from(self.x).expect("patch starts within validated width") * 3;
        out.clear();
        out.reserve(row_len * height);
        for row in top..top + height {
            out.extend_from_slice(&image[row * stride + start..row * stride + start + row_len]);
        }
    }
}

fn move_cursor(out: &mut Vec<u8>, x: u32, y: u32, cell_size: Option<(u16, u16)>) {
    let (cell_w, cell_h) =
        cell_size.map_or((1, 1), |(w, h)| (u32::from(w).max(1), u32::from(h).max(1)));
    let col = x / cell_w + 1;
    let row = y / cell_h + 1;
    write!(out, "\x1b[{row};{col}H").expect("writing to Vec cannot fail");
}

/// Encode a complete immutable object without reading or copying its pixels.
///
/// The caller must retain the object's producer guard until the terminal opens
/// it, and must restore retained RGB pixels before using incremental patches.
pub fn present_shared(frame: &PublishedFrame, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(b"\x1b[?2026h");
    delete_all(out);
    out.extend_from_slice(b"\x1b[2J\x1b[H\x1b_G");
    transmit(
        out,
        SCREEN_ID,
        frame.size().width(),
        frame.size().height(),
        Placement::Whole,
    );
    out.extend_from_slice(b",t=s;");
    out.extend_from_slice(STANDARD.encode(frame.name()).as_bytes());
    out.extend_from_slice(b"\x1b\\\x1b[?2026l");
}

fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").expect("writing to Vec cannot fail");
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Placement {
    Whole,
    Patch,
}

/// Common kitty transmission keys; callers append transport and chunk keys.
pub(crate) fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, placement: Placement) {
    let (placement, z) = match placement {
        Placement::Whole => (0, 1),
        Placement::Patch => (1, 2),
    };
    write!(
        out,
        "a=T,f=24,s={width},v={height},i={id},p={placement},z={z},C=1,q=2"
    )
    .expect("writing to Vec cannot fail");
}
