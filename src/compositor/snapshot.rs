//! Bounded, owned copies of committed Wayland shm buffers.

use smithay::reexports::wayland_server::protocol::wl_shm;

use crate::pixels::FrameSize;

#[derive(Debug)]
pub(super) struct Snapshot {
    pub(super) width: u32,
    pub(super) height: u32,
    /// `r, g, b, a` per pixel, premultiplied as Wayland defines it.
    pub(super) pixels: Vec<u8>,
    pub(super) opaque: bool,
}

impl Snapshot {
    /// Copy a committed shm buffer into an owned RGBA snapshot, reusing
    /// `pixels` when possible. On failure, `pixels` remains with the
    /// caller.
    #[expect(
        unsafe_code,
        reason = "the shm pool is only reachable as a raw pointer, so reading it takes one \
              bounded, documented slice"
    )]
    pub(super) fn copy_shm(
        ptr: *const u8,
        len: usize,
        data: &smithay::wayland::shm::BufferData,
        bound: Option<(u32, u32)>,
        pixels: &mut Vec<u8>,
    ) -> Option<Self> {
        if !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        ) || data.width <= 0
            || data.height <= 0
            || data.stride <= 0
        {
            return None;
        }
        let size = FrameSize::new(data.width as u32, data.height as u32)?;
        let stride = usize::try_from(data.stride).ok()?;
        let offset = usize::try_from(data.offset).ok()?;
        let row_bytes = size.rgba_stride();
        if stride < row_bytes || !size.fits(bound) {
            return None;
        }
        if offset.checked_add(stride.checked_mul(size.height() as usize)?)? > len {
            return None;
        }
        let source = unsafe { std::slice::from_raw_parts(ptr, len) };
        Self::resize_pixels(pixels, size);
        let opaque = if data.format == wl_shm::Format::Xrgb8888 {
            Self::copy_rows::<true>(&source[offset..], stride, row_bytes, pixels)
        } else {
            Self::copy_rows::<false>(&source[offset..], stride, row_bytes, pixels)
        };
        Some(Self {
            width: size.width(),
            height: size.height(),
            pixels: std::mem::take(pixels),
            opaque,
        })
    }

    /// Reuse RGBA storage without retaining oversized allocations after a
    /// shrink.
    pub(super) fn resize_pixels(pixels: &mut Vec<u8>, size: FrameSize) {
        let needed = size.rgba_len();
        pixels.resize(needed, 0);
        if pixels.capacity() >= needed.saturating_mul(4).max(1 << 20) {
            pixels.shrink_to_fit();
        }
    }

    fn copy_rows<const XRGB: bool>(
        source: &[u8],
        stride: usize,
        row_bytes: usize,
        pixels: &mut [u8],
    ) -> bool {
        let mut opaque = true;
        for (source_row, row) in source
            .chunks_exact(stride)
            .zip(pixels.chunks_exact_mut(row_bytes))
        {
            let source_row = source_row[..row_bytes].as_chunks::<4>().0;
            let row = row.as_chunks_mut::<4>().0;
            for (pixel, out_pixel) in source_row.iter().zip(row) {
                let alpha = if XRGB { 255 } else { pixel[3] };
                *out_pixel = [pixel[2], pixel[1], pixel[0], alpha];
                if !XRGB {
                    opaque &= alpha == 255;
                }
            }
        }
        opaque
    }
}
