//! Bounded, owned copies of committed Wayland shm buffers.

use meowland_core::pixels::FrameSize;
use smithay::reexports::wayland_server::protocol::wl_shm;

#[derive(Debug)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub opaque: bool,
}

impl Snapshot {
    /// Copy a committed shm buffer, reusing the previous snapshot's storage.
    /// On invalid input, the previous snapshot remains intact.
    ///
    /// # Safety
    /// The pointer must remain readable for `len` bytes throughout this call.
    /// The source may be modified externally; no shared references are created.
    pub unsafe fn copy_shm(
        ptr: *const u8,
        len: usize,
        data: &smithay::wayland::shm::BufferData,
        bound: Option<(u32, u32)>,
        previous: &mut Option<Self>,
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
        let mut pixels = previous
            .take()
            .map_or_else(Vec::new, |previous| previous.pixels);
        pixels.resize(size.rgba_len(), 0);
        if pixels.capacity() >= pixels.len().saturating_mul(4).max(1 << 20) {
            pixels.shrink_to_fit();
        }
        // SAFETY: The caller retains the mapping; offset, stride, and all rows
        // fit within its checked length. The destination owns exactly one
        // frame.
        let opaque = unsafe {
            if data.format == wl_shm::Format::Xrgb8888 {
                Self::copy_rows::<true>(ptr.add(offset), stride, row_bytes, &mut pixels)
            } else {
                Self::copy_rows::<false>(ptr.add(offset), stride, row_bytes, &mut pixels)
            }
        };
        Some(Self {
            width: size.width(),
            height: size.height(),
            pixels,
            opaque,
        })
    }

    unsafe fn copy_rows<const XRGB: bool>(
        source: *const u8,
        stride: usize,
        row_bytes: usize,
        pixels: &mut [u8],
    ) -> bool {
        let mut opaque = true;
        for (y, row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
            for (x, out_pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                // SAFETY: The caller checked every pixel against the live
                // mapping. Word reads require the checked alignment; byte
                // reads handle unaligned pixels. Neither creates references
                // to externally writable memory.
                let pixel = unsafe {
                    let pixel = source.add(y * stride + x * 4);
                    #[expect(clippy::cast_ptr_alignment)]
                    let word = pixel.cast::<u32>();
                    if word.is_aligned() {
                        word.read_volatile().to_ne_bytes()
                    } else {
                        [
                            pixel.read_volatile(),
                            pixel.add(1).read_volatile(),
                            pixel.add(2).read_volatile(),
                            pixel.add(3).read_volatile(),
                        ]
                    }
                };
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
