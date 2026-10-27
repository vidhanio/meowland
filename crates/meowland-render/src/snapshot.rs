//! Bounded, owned copies of committed Wayland shm buffers.

use meowland_core::pixels::FrameSize;
use smithay::reexports::wayland_server::protocol::wl_shm;

/// An owned, tightly packed RGBA image copied from a validated client buffer.
#[derive(Debug)]
pub struct Snapshot {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
    pub(crate) opaque: bool,
}

impl Snapshot {
    /// Create the one-pixel snapshot behind a `wp_single_pixel_buffer`.
    #[must_use]
    pub fn single_pixel(pixel: [u8; 4]) -> Self {
        Self {
            width: 1,
            height: 1,
            pixels: pixel.to_vec(),
            opaque: pixel[3] == 255,
        }
    }

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
        let size = FrameSize::new(
            u32::try_from(data.width).ok()?,
            u32::try_from(data.height).ok()?,
        )?;
        let stride = usize::try_from(data.stride).ok()?;
        let offset = usize::try_from(data.offset).ok()?;
        let row_bytes = size.rgba_stride();
        if stride < row_bytes || !size.fits(bound) {
            return None;
        }
        if offset.checked_add(stride.checked_mul(usize::try_from(size.height()).ok()?)?)? > len {
            return None;
        }
        let mut pixels = previous
            .take()
            .map_or_else(Vec::new, |previous| previous.pixels);
        pixels.resize(size.rgba_len(), 0);
        if pixels.capacity() >= pixels.len().saturating_mul(4).max(1 << 20) {
            pixels.shrink_to_fit();
        }
        let source = ptr.wrapping_add(offset);
        let opaque = if data.format == wl_shm::Format::Xrgb8888 {
            // SAFETY: The caller retains the mapping, all source rows fit
            // within its checked length, and pixels owns exactly
            // one destination frame.
            unsafe { Self::copy_rows::<true>(source, stride, row_bytes, &mut pixels) }
        } else {
            // SAFETY: The same checked source bounds and owned destination
            // apply to ARGB; only alpha interpretation differs from
            // the XRGB path.
            unsafe { Self::copy_rows::<false>(source, stride, row_bytes, &mut pixels) }
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
                let pixel = source.wrapping_add(y * stride + x * 4);
                // SAFETY: The caller checked all four bytes against the live
                // mapping.
                let pixel = unsafe { read_pixel(pixel) };
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

/// Read one pixel without forming a reference to externally writable memory.
///
/// # Safety
/// `source` must remain readable for four bytes throughout this call.
#[inline]
unsafe fn read_pixel(source: *const u8) -> [u8; 4] {
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "the u32 pointer is dereferenced only after checking its alignment"
    )]
    let aligned = source.cast::<u32>();
    if aligned.is_aligned() {
        // SAFETY: Alignment is checked and the caller guarantees four readable
        // bytes.
        unsafe { aligned.read_volatile() }.to_ne_bytes()
    } else {
        std::array::from_fn(|channel| {
            let component = source.wrapping_add(channel);
            // SAFETY: Each byte lies within the caller's readable pixel; u8
            // needs no alignment.
            unsafe { component.read_volatile() }
        })
    }
}
