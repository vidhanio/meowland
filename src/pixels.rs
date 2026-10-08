//! Validated dimensions shared by surface snapshots and terminal frames.

use std::{num::NonZeroU32, ops::Range};

pub const MAX_SURFACE_SIDE: u32 = 8192;
pub const MAX_SURFACE_PIXELS: usize = 16_000_000;

/// Nonempty dimensions whose pixel count and byte strides fit in `usize`.
/// Keep the fields private so all allocation sizes come from checked bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSize {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl FrameSize {
    pub(crate) const fn new(width: u32, height: u32) -> Option<Self> {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return None;
        };
        if width > MAX_SURFACE_SIDE
            || height > MAX_SURFACE_SIDE
            || width as u64 * height as u64 > MAX_SURFACE_PIXELS as u64
        {
            return None;
        }
        Some(Self {
            width: w,
            height: h,
        })
    }

    pub(crate) const fn width(self) -> u32 {
        self.width.get()
    }

    pub(crate) const fn height(self) -> u32 {
        self.height.get()
    }

    pub(crate) const fn rgb_stride(self) -> usize {
        self.width.get() as usize * 3
    }

    pub(crate) const fn rgb_len(self) -> usize {
        self.rgb_stride() * self.height.get() as usize
    }

    pub(crate) const fn rgba_stride(self) -> usize {
        self.width.get() as usize * 4
    }

    pub(crate) const fn rgba_len(self) -> usize {
        self.rgba_stride() * self.height.get() as usize
    }

    pub(crate) fn fits(self, bound: Option<(u32, u32)>) -> bool {
        bound.is_none_or(|(width, height)| self.width.get() <= width && self.height.get() <= height)
    }

    /// Validate a nonempty RGB row band before indexing the composed frame.
    pub(crate) fn rgb_band(self, y: u32, bytes: usize) -> Option<Range<usize>> {
        let stride = self.rgb_stride();
        if y >= self.height.get() || bytes == 0 || !bytes.is_multiple_of(stride) {
            return None;
        }
        let start = y as usize * stride;
        let end = start.checked_add(bytes)?;
        (end <= self.rgb_len()).then_some(start..end)
    }
}
