//! Bounded, owned copies of committed Wayland shm buffers.

use smithay::reexports::wayland_server::protocol::wl_shm;

use super::{MAX_SURFACE_PIXELS, MAX_SURFACE_SIDE};

#[derive(Debug)]
pub(super) struct Snapshot {
    pub(super) width: u32,
    pub(super) height: u32,
    /// `r, g, b, a` per pixel, premultiplied as Wayland defines it.
    pub(super) pixels: Vec<u8>,
    pub(super) opaque: bool,
}

/// Copy a committed shm buffer into an owned RGBA snapshot, reusing `pixels`
/// when possible. On failure, `pixels` remains with the caller.
#[expect(
    unsafe_code,
    reason = "the shm pool is only reachable as a raw pointer, so reading it takes one \
              bounded, documented slice"
)]
pub(super) fn copy_buffer(
    ptr: *const u8,
    len: usize,
    data: &smithay::wayland::shm::BufferData,
    bound: Option<(u32, u32)>,
    pixels: &mut Vec<u8>,
) -> Option<Snapshot> {
    if !matches!(
        data.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) || data.width <= 0
        || data.height <= 0
        || data.stride <= 0
    {
        return None;
    }
    let width = data.width as u32;
    let height = data.height as u32;
    let stride = data.stride as usize;
    let offset = data.offset as usize;
    if width > MAX_SURFACE_SIDE
        || height > MAX_SURFACE_SIDE
        || width as usize * height as usize > MAX_SURFACE_PIXELS
        || stride < width as usize * 4
    {
        return None;
    }
    if let Some((max_width, max_height)) = bound
        && (width > max_width || height > max_height)
    {
        return None;
    }
    if offset.checked_add(stride.checked_mul(height as usize)?)? > len {
        return None;
    }
    // SAFETY: `with_buffer_contents` supplies the pool mapping and its length.
    // The checked offset, stride and height keep every pixel read in bounds.
    // The client may modify the mapping; no reference to it escapes this copy.
    let source = unsafe { std::slice::from_raw_parts(ptr, len) };
    let row_bytes = width as usize * 4;
    let needed = row_bytes * height as usize;
    pixels.resize(needed, 0);
    // Do not retain a much larger allocation after a surface shrinks.
    if pixels.capacity() >= needed.saturating_mul(4).max(1 << 20) {
        pixels.shrink_to_fit();
    }
    let opaque = if data.format == wl_shm::Format::Xrgb8888 {
        copy_rows::<true>(&source[offset..], stride, row_bytes, pixels)
    } else {
        copy_rows::<false>(&source[offset..], stride, row_bytes, pixels)
    };
    Some(Snapshot {
        width,
        height,
        pixels: std::mem::take(pixels),
        opaque,
    })
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
#[cfg(test)]
mod tests {
    use super::*;

    /// Padding must not enter the snapshot, even when reusing storage.
    #[test]
    fn snapshot_swizzles_both_shm_formats() {
        const STRIDE: usize = 12;
        let raw: [u8; 24] = [
            3, 4, 5, 0x40, 6, 7, 8, 0xff, 0xbb, 0xbb, 0xbb, 0xbb, 9, 10, 11, 0xff, 12, 13, 14,
            0x7f, 0xbb, 0xbb, 0xbb, 0xbb,
        ];
        let data = |format| smithay::wayland::shm::BufferData {
            format,
            width: 2,
            height: 2,
            stride: STRIDE as i32,
            offset: 0,
        };
        let copy = |format, reuse: Vec<u8>| {
            let mut reuse = reuse;
            copy_buffer(raw.as_ptr(), raw.len(), &data(format), None, &mut reuse)
                .expect("a 2x2 buffer")
        };

        let argb = copy(wl_shm::Format::Argb8888, Vec::new());
        assert_eq!(
            argb.pixels,
            [
                5, 4, 3, 0x40, 8, 7, 6, 0xff, 11, 10, 9, 0xff, 14, 13, 12, 0x7f
            ]
        );
        assert!(!argb.opaque, "an alpha byte below 255 is not opaque");

        let xrgb = copy(wl_shm::Format::Xrgb8888, Vec::new());
        assert_eq!(
            xrgb.pixels,
            [
                5, 4, 3, 0xff, 8, 7, 6, 0xff, 11, 10, 9, 0xff, 14, 13, 12, 0xff
            ]
        );
        assert!(xrgb.opaque, "Xrgb8888 has no alpha byte");

        let mut reused = argb.pixels.clone();
        reused.fill(0xcc);
        let again = copy(wl_shm::Format::Argb8888, reused);
        assert_eq!(again.pixels, argb.pixels);
    }
}
