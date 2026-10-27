//! Validated dimensions shared by surface snapshots and terminal frames.

use std::{num::NonZeroU16, ops::Range};

/// Maximum supported width or height; bounds per-row work and graphics
/// allocations.
pub const MAX_SURFACE_SIDE: u32 = 8192;
/// Maximum pixels per frame; bounds RGB and RGBA allocations independently of
/// aspect ratio.
pub const MAX_SURFACE_PIXELS: usize = 16_000_000;

/// Nonempty dimensions whose pixel count and byte strides fit in `usize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSize {
    width: NonZeroU16,
    height: NonZeroU16,
}

impl FrameSize {
    /// Validate nonempty dimensions against the surface limits.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Option<Self> {
        if width > MAX_SURFACE_SIDE || height > MAX_SURFACE_SIDE {
            return None;
        }
        let width = NonZeroU16::new(u16::try_from(width).ok()?)?;
        let height = NonZeroU16::new(u16::try_from(height).ok()?)?;
        if usize::from(width.get()) * usize::from(height.get()) > MAX_SURFACE_PIXELS {
            return None;
        }
        Some(Self { width, height })
    }

    /// Width in physical pixels, always nonzero and within
    /// [`MAX_SURFACE_SIDE`].
    #[must_use]
    pub fn width(self) -> u32 {
        u32::from(self.width.get())
    }

    /// Height in physical pixels, always nonzero and within
    /// [`MAX_SURFACE_SIDE`].
    #[must_use]
    pub fn height(self) -> u32 {
        u32::from(self.height.get())
    }

    /// Bytes in one tightly packed, three-channel RGB row.
    #[must_use]
    pub fn rgb_stride(self) -> usize {
        usize::from(self.width.get()) * 3
    }

    /// Bytes required for a tightly packed RGB frame.
    #[must_use]
    pub fn rgb_len(self) -> usize {
        self.rgb_stride() * usize::from(self.height.get())
    }

    /// Bytes in one tightly packed, four-channel RGBA row.
    #[must_use]
    pub fn rgba_stride(self) -> usize {
        usize::from(self.width.get()) * 4
    }

    /// Bytes required for a tightly packed RGBA frame.
    #[must_use]
    pub fn rgba_len(self) -> usize {
        self.rgba_stride() * usize::from(self.height.get())
    }

    /// Whether these dimensions fit the optional physical-pixel bounds.
    #[must_use]
    pub fn fits(self, bound: Option<(u32, u32)>) -> bool {
        bound.is_none_or(|(width, height)| self.width() <= width && self.height() <= height)
    }

    /// Validate a nonempty RGB row band before indexing the composed frame.
    #[must_use]
    pub fn rgb_band(self, y: u32, bytes: usize) -> Option<Range<usize>> {
        let stride = self.rgb_stride();
        if y >= self.height() || bytes == 0 || !bytes.is_multiple_of(stride) {
            return None;
        }
        let start = usize::try_from(y).ok()? * stride;
        let end = start.checked_add(bytes)?;
        (end <= self.rgb_len()).then_some(start..end)
    }
}
