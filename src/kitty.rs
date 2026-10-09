//! Encode RGB frames for the kitty graphics protocol.

use std::{
    fs::File,
    io::{self, Write},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustix::{fs::Mode, io::Errno, shm};

use crate::pixels::FrameSize;

mod encoding;
use encoding::{Encoder, Encoding};

const MAX_PATCHES: usize = 32;
/// Consecutive unread frames before abandoning shared memory permanently.
const DROP_LIMIT: u32 = 30;
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
        let mut file = self.create().ok()?;
        if file.write_all(&[0; 3]).is_err() {
            self.clear();
            return None;
        }
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
    dropped: bool,
    drops: u32,
}

#[derive(Debug)]
struct Frame {
    size: FrameSize,
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
    Unread,
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
    #[inline]
    pub fn present(&mut self, width: u32, height: u32, y: u32, band: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        self.present_into(width, height, y, band, &mut out);
        out
    }

    /// Compose and encode into reusable output storage. Clears `out`, retaining
    /// its capacity, even when the band is invalid, unchanged, or dropped.
    pub fn present_into(
        &mut self,
        width: u32,
        height: u32,
        y: u32,
        band: Vec<u8>,
        out: &mut Vec<u8>,
    ) {
        out.clear();
        self.dropped = false;
        let Some(changed) = self.apply_band(width, height, y, band) else {
            return;
        };
        if !changed && self.display_current {
            return;
        }

        if self.patch_count == 0
            && self.base.as_ref().is_some_and(|old| {
                Some(old.size) == self.size
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
            return;
        }

        let patches = self.select_patches();

        let file = if patches {
            None
        } else {
            match self.ready_shared() {
                SharedFrame::Inline => None,
                SharedFrame::Ready(file) => Some(file),
                SharedFrame::Unread => return,
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
    }

    /// Drop frames while the terminal is briefly behind, then permanently
    /// fall back to pty transfer rather than cycling through freezes.
    fn ready_shared(&mut self) -> SharedFrame {
        let Some(slot) = &self.shared else {
            return SharedFrame::Inline;
        };
        match slot.create() {
            Ok(file) => {
                self.drops = 0;
                return SharedFrame::Ready(file);
            }
            Err(error) if error.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {}
            Err(error) => {
                tracing::warn!(%error, "Shared-memory transport unavailable; using inline frames");
                self.shared = None;
                self.drops = 0;
                return SharedFrame::Inline;
            }
        }
        self.drops += 1;
        if self.drops < DROP_LIMIT {
            self.dropped = true;
            self.display_current = false;
            return SharedFrame::Unread;
        }
        self.drops = 0;
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
            self.encoder
                .image(out, id, (rect.width, rect.height), &self.patch_pixels, true);
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
                    false,
                    encoding,
                );
            } else {
                self.encoder
                    .image(out, SCREEN_ID, (width, height), &self.image, false);
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

    fn apply_band(&mut self, width: u32, height: u32, y: u32, band: Vec<u8>) -> Option<bool> {
        let size = FrameSize::new(width, height)?;
        let bytes = size.rgb_band(y, band.len())?;
        let (start, end) = (bytes.start, bytes.end);
        let stride = size.rgb_stride();
        let rows = height as usize;

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
            self.size = Some(size);
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
            let row_start = y as usize * width as usize * 3;
            let row_end = (y + h) as usize * width as usize * 3;
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
        for row in y as usize..(y + height) as usize {
            let start = x as usize * 3;
            let end = (x + width) as usize * 3;
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
        let stride = width as usize * 3;
        let row_len = self.width as usize * 3;
        out.clear();
        out.reserve(row_len * self.height as usize);
        for row in self.y as usize..(self.y + self.height) as usize {
            let start = self.x as usize * 3;
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

fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").expect("writing to Vec cannot fail");
}

/// Common kitty transmission keys; callers append transport and chunk keys.
fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, placement: u32) {
    let z = if placement == 0 { 1 } else { 2 };
    write!(
        out,
        "a=T,f=24,s={width},v={height},i={id},p={placement},z={z},C=1,q=2"
    )
    .expect("writing to Vec cannot fail");
}
