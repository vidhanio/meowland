//! Bounded, owned copies of committed Wayland shm buffers.

use meowland_core::pixels::FrameSize;
use smithay::{
    reexports::wayland_server::protocol::wl_shm,
    utils::{Buffer, Rectangle, Size},
};

/// How imported RGBA pixels treat the alpha channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opacity {
    /// Keep the source alpha channel.
    Source,
    /// Replace the alpha channel with full opacity.
    Force,
}

/// Layout that lets a previous snapshot be patched in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Source {
    format: wl_shm::Format,
    stride: usize,
}

/// An owned, tightly packed RGBA image copied from a validated client buffer.
#[derive(Debug)]
pub struct Snapshot {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
    transparent: usize,
    source: Option<Source>,
}

impl Snapshot {
    /// Create the one-pixel snapshot behind a `wp_single_pixel_buffer`.
    #[must_use]
    pub fn single_pixel(pixel: [u8; 4]) -> Self {
        Self::from_rgba(1, 1, pixel.to_vec(), Opacity::Source)
    }

    /// Build a snapshot from tightly packed RGBA pixels.
    ///
    /// [`Opacity::Force`] replaces the alpha channel with full opacity, which
    /// matches the format guarantee of DMA-BUF imports. Transparency is
    /// counted once here so later queries stay cheap.
    pub(crate) fn from_rgba(
        width: u32,
        height: u32,
        mut pixels: Vec<u8>,
        opacity: Opacity,
    ) -> Self {
        let mut transparent = 0;
        for pixel in pixels.as_chunks_mut::<4>().0 {
            if opacity == Opacity::Force {
                pixel[3] = 255;
            } else if pixel[3] != 255 {
                transparent += 1;
            }
        }
        Self {
            width,
            height,
            pixels,
            transparent,
            source: None,
        }
    }

    /// Whether every pixel is fully opaque.
    pub(crate) const fn opaque(&self) -> bool {
        self.transparent == 0
    }

    /// Copy a committed shm buffer, reusing the previous snapshot's storage.
    ///
    /// When the previous snapshot has the same size, format, and stride, only
    /// the pixels inside `damage` (in buffer coordinates) are re-read and
    /// swizzled. Otherwise, or when `damage` is empty, the whole image is
    /// copied. On invalid input, the previous snapshot remains intact.
    ///
    /// The opaque hint is derived from a count of non-opaque pixels that is
    /// updated per damaged pixel, so patched snapshots stay exact without
    /// rescanning the image.
    ///
    /// # Safety
    /// The pointer must remain readable for `len` bytes throughout this call.
    /// The source may be modified externally; no shared references are created.
    pub unsafe fn copy_shm(
        ptr: *const u8,
        len: usize,
        data: &smithay::wayland::shm::BufferData,
        bound: Option<(u32, u32)>,
        damage: &[Rectangle<i32, Buffer>],
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
        let source = Source {
            format: data.format,
            stride,
        };
        let reusable = !damage.is_empty()
            && previous.as_ref().is_some_and(|previous| {
                previous.source == Some(source)
                    && previous.width == size.width()
                    && previous.height == size.height()
            });
        let source_ptr = ptr.wrapping_add(offset);
        let Some(mut snapshot) = reusable.then(|| previous.take()).flatten() else {
            let mut pixels = Self::reclaim(previous.take(), size);
            let transparent = if data.format == wl_shm::Format::Xrgb8888 {
                // SAFETY: Every source row lies within the checked mapping and
                // pixels owns exactly one destination frame.
                unsafe { Self::copy_rows::<true>(source_ptr, stride, row_bytes, &mut pixels) }
            } else {
                // SAFETY: The same checked source bounds and owned destination
                // apply to ARGB; only alpha interpretation differs.
                unsafe { Self::copy_rows::<false>(source_ptr, stride, row_bytes, &mut pixels) }
            };
            return Some(Self {
                width: size.width(),
                height: size.height(),
                pixels,
                transparent,
                source: Some(source),
            });
        };
        let area = Rectangle::from_size(Size::from((
            i32::try_from(size.width()).ok()?,
            i32::try_from(size.height()).ok()?,
        )));
        for damage in damage {
            let Some(damage) = damage.intersection(area) else {
                continue;
            };
            if damage.size.w <= 0 || damage.size.h <= 0 {
                continue;
            }
            if data.format == wl_shm::Format::Xrgb8888 {
                // SAFETY: The clamped rectangle lies inside the checked source
                // rows and the owned destination frame.
                unsafe {
                    Self::copy_rect::<true>(
                        source_ptr,
                        stride,
                        row_bytes,
                        &mut snapshot.pixels,
                        damage,
                        &mut snapshot.transparent,
                    );
                }
            } else {
                // SAFETY: Same bounds as the XRGB path; alpha is counted while
                // copying.
                unsafe {
                    Self::copy_rect::<false>(
                        source_ptr,
                        stride,
                        row_bytes,
                        &mut snapshot.pixels,
                        damage,
                        &mut snapshot.transparent,
                    );
                }
            }
        }
        snapshot.source = Some(source);
        Some(snapshot)
    }

    /// Reuse a previous snapshot's storage for a new tightly packed image.
    pub(crate) fn reclaim(previous: Option<Self>, size: FrameSize) -> Vec<u8> {
        let mut pixels = previous.map_or_else(Vec::new, |previous| previous.pixels);
        pixels.resize(size.rgba_len(), 0);
        if pixels.capacity() >= pixels.len().saturating_mul(4).max(1 << 20) {
            pixels.shrink_to_fit();
        }
        pixels
    }

    unsafe fn copy_rows<const XRGB: bool>(
        source: *const u8,
        stride: usize,
        row_bytes: usize,
        pixels: &mut [u8],
    ) -> usize {
        let mut transparent = 0;
        for (y, row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
            for (x, out_pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let pixel = source.wrapping_add(y * stride + x * 4);
                // SAFETY: The caller checked all four bytes against the live
                // mapping.
                let pixel = unsafe { read_pixel(pixel) };
                let alpha = if XRGB { 255 } else { pixel[3] };
                *out_pixel = [pixel[2], pixel[1], pixel[0], alpha];
                if !XRGB && alpha != 255 {
                    transparent += 1;
                }
            }
        }
        transparent
    }

    /// Swizzle one damaged rectangle, keeping the transparency count exact.
    ///
    /// # Safety
    /// `damage` must lie inside the source and destination frames.
    unsafe fn copy_rect<const XRGB: bool>(
        source: *const u8,
        stride: usize,
        row_bytes: usize,
        pixels: &mut [u8],
        damage: Rectangle<i32, Buffer>,
        transparent: &mut usize,
    ) {
        let x0 = usize::try_from(damage.loc.x).expect("clamped damage x is non-negative");
        let y0 = usize::try_from(damage.loc.y).expect("clamped damage y is non-negative");
        let width = usize::try_from(damage.size.w).expect("clamped damage width is positive");
        let height = usize::try_from(damage.size.h).expect("clamped damage height is positive");
        for y in y0..y0 + height {
            let row_start = y * row_bytes;
            let row = &mut pixels[row_start..row_start + row_bytes];
            let source_row = source.wrapping_add(y * stride);
            for (x, out_pixel) in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .enumerate()
                .skip(x0)
                .take(width)
            {
                let pixel = source_row.wrapping_add(x * 4);
                // SAFETY: The caller guarantees the damaged rectangle lies
                // inside both frames.
                let pixel = unsafe { read_pixel(pixel) };
                if XRGB {
                    *out_pixel = [pixel[2], pixel[1], pixel[0], 255];
                } else {
                    if out_pixel[3] != 255 {
                        *transparent -= 1;
                    }
                    if pixel[3] != 255 {
                        *transparent += 1;
                    }
                    *out_pixel = [pixel[2], pixel[1], pixel[0], pixel[3]];
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use smithay::wayland::shm::BufferData;

    use super::*;

    fn buffer_data(
        format: wl_shm::Format,
        width: i32,
        height: i32,
        stride: i32,
        offset: i32,
    ) -> BufferData {
        BufferData {
            offset,
            width,
            height,
            stride,
            format,
        }
    }

    fn pixels(pixels: &[[u8; 4]]) -> Vec<u8> {
        pixels.iter().flatten().copied().collect()
    }

    fn damage(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Buffer> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    fn copy(
        source: &[u8],
        data: &BufferData,
        damage: &[Rectangle<i32, Buffer>],
        previous: &mut Option<Snapshot>,
        bound: Option<(u32, u32)>,
    ) -> Option<Snapshot> {
        // SAFETY: `source` stays alive and unmodified for the duration of the
        // call.
        unsafe { Snapshot::copy_shm(source.as_ptr(), source.len(), data, bound, damage, previous) }
    }

    #[test]
    fn copy_swizzles_channels_and_forces_xrgb_alpha() {
        let source = pixels(&[[1, 2, 3, 255], [4, 5, 6, 128], [7, 8, 9, 0]]);
        let data = buffer_data(wl_shm::Format::Argb8888, 3, 1, 12, 0);
        let snapshot = copy(&source, &data, &[], &mut None, None).unwrap();

        assert_eq!(
            snapshot.pixels,
            pixels(&[[3, 2, 1, 255], [6, 5, 4, 128], [9, 8, 7, 0]])
        );
        assert!(!snapshot.opaque());

        let source = pixels(&[[10, 20, 30, 0], [40, 50, 60, 77]]);
        let data = buffer_data(wl_shm::Format::Xrgb8888, 2, 1, 8, 0);
        let snapshot = copy(&source, &data, &[], &mut None, None).unwrap();

        assert_eq!(
            snapshot.pixels,
            pixels(&[[30, 20, 10, 255], [60, 50, 40, 255]])
        );
        assert!(snapshot.opaque());
    }

    #[test]
    fn partial_damage_patches_only_the_rectangle_and_updates_transparency() {
        let source = pixels(&[[10, 20, 30, 255]; 8]);
        let data = buffer_data(wl_shm::Format::Argb8888, 4, 2, 16, 0);
        let mut previous = copy(&source, &data, &[], &mut None, None);
        assert!(previous.as_ref().unwrap().opaque());

        let mut changed = source;
        changed[4..8].copy_from_slice(&[50, 60, 70, 0]);
        changed[8..12].copy_from_slice(&[80, 90, 100, 128]);
        changed[16..20].copy_from_slice(&[1, 2, 3, 0]);
        let patched = copy(&changed, &data, &[damage(1, 0, 2, 1)], &mut previous, None).unwrap();

        assert_eq!(
            patched.pixels,
            pixels(&[
                [30, 20, 10, 255],
                [70, 60, 50, 0],
                [100, 90, 80, 128],
                [30, 20, 10, 255],
                [30, 20, 10, 255],
                [30, 20, 10, 255],
                [30, 20, 10, 255],
                [30, 20, 10, 255],
            ])
        );
        assert!(!patched.opaque());

        let restored = pixels(&[[7, 8, 9, 255]; 8]);
        let restored = copy(
            &restored,
            &data,
            &[damage(0, 0, 4, 2)],
            &mut Some(patched),
            None,
        )
        .unwrap();
        assert!(restored.opaque());
        assert_eq!(restored.pixels, pixels(&[[9, 8, 7, 255]; 8]));
    }

    #[test]
    fn empty_damage_forces_a_full_copy() {
        let source = pixels(&[[1, 2, 3, 0], [4, 5, 6, 128]]);
        let data = buffer_data(wl_shm::Format::Argb8888, 2, 1, 8, 0);
        let mut previous = copy(&source, &data, &[], &mut None, None);
        assert!(!previous.as_ref().unwrap().opaque());

        let changed = pixels(&[[9, 8, 7, 255], [6, 5, 4, 255]]);
        let snapshot = copy(&changed, &data, &[], &mut previous, None).unwrap();

        assert_eq!(snapshot.pixels, pixels(&[[7, 8, 9, 255], [4, 5, 6, 255]]));
        assert!(snapshot.opaque());
    }

    #[test]
    fn invalid_buffers_leave_the_previous_snapshot_untouched() {
        let source = pixels(&[[1, 2, 3, 255], [4, 5, 6, 128]]);
        let data = buffer_data(wl_shm::Format::Argb8888, 2, 1, 8, 0);
        let mut previous = copy(&source, &data, &[], &mut None, None);
        let before = previous.as_ref().unwrap().pixels.clone();

        let cases = [
            buffer_data(wl_shm::Format::Rgb565, 2, 1, 8, 0),
            buffer_data(wl_shm::Format::Argb8888, 0, 1, 8, 0),
            buffer_data(wl_shm::Format::Argb8888, 2, -1, 8, 0),
            buffer_data(wl_shm::Format::Argb8888, 2, 1, 4, 0),
            buffer_data(wl_shm::Format::Argb8888, 2, 1, 8, -1),
            buffer_data(wl_shm::Format::Argb8888, 2, 1, 8, 100),
        ];
        for case in &cases {
            assert!(
                copy(&source, case, &[], &mut previous, None).is_none(),
                "{case:?}"
            );
            assert_eq!(previous.as_ref().unwrap().pixels, before, "{case:?}");
        }

        assert!(
            copy(&source[..7], &data, &[], &mut previous, None).is_none(),
            "truncated buffer"
        );
        assert_eq!(previous.as_ref().unwrap().pixels, before);

        assert!(
            copy(&source, &data, &[], &mut previous, Some((1, 1))).is_none(),
            "bound smaller than the buffer"
        );
        assert_eq!(previous.as_ref().unwrap().pixels, before);
    }

    #[test]
    fn format_or_stride_changes_force_a_full_copy() {
        let source = pixels(&[[10, 20, 30, 255]; 4]);
        let data = buffer_data(wl_shm::Format::Argb8888, 2, 2, 8, 0);
        let mut previous = copy(&source, &data, &[], &mut None, None);
        assert!(previous.as_ref().unwrap().opaque());

        let changed = pixels(&[[1, 2, 3, 0]; 4]);
        let xrgb = buffer_data(wl_shm::Format::Xrgb8888, 2, 2, 8, 0);
        let snapshot = copy(&changed, &xrgb, &[damage(0, 0, 1, 1)], &mut previous, None).unwrap();
        assert!(snapshot.opaque());
        assert_eq!(snapshot.pixels, pixels(&[[3, 2, 1, 255]; 4]));

        let mut previous = Some(snapshot);
        let changed = pixels(&[[9, 8, 7, 0]; 4]);
        let mut padded = Vec::with_capacity(24);
        for _ in 0..2 {
            padded.extend_from_slice(&changed[..8]);
            padded.extend_from_slice(&[0xAA; 4]);
        }
        let padded_data = buffer_data(wl_shm::Format::Xrgb8888, 2, 2, 12, 0);
        let snapshot = copy(
            &padded,
            &padded_data,
            &[damage(0, 0, 1, 1)],
            &mut previous,
            None,
        )
        .unwrap();
        assert_eq!(snapshot.pixels, pixels(&[[7, 8, 9, 255]; 4]));
    }

    #[test]
    fn copy_respects_offset_and_stride_padding() {
        let mut source = vec![0xAA; 3];
        source.extend_from_slice(&[1, 2, 3, 255]);
        source.extend_from_slice(&[4, 5, 6, 128]);
        source.extend_from_slice(&[0xEE; 4]);
        source.extend_from_slice(&[7, 8, 9, 0]);
        source.extend_from_slice(&[10, 11, 12, 255]);
        source.extend_from_slice(&[0xEE; 4]);
        let data = buffer_data(wl_shm::Format::Argb8888, 2, 2, 12, 3);
        let snapshot = copy(&source, &data, &[], &mut None, None).unwrap();

        assert_eq!(
            snapshot.pixels,
            pixels(&[
                [3, 2, 1, 255],
                [6, 5, 4, 128],
                [9, 8, 7, 0],
                [12, 11, 10, 255],
            ])
        );
        assert!(!snapshot.opaque());
    }

    #[test]
    fn damage_is_clipped_to_the_buffer() {
        let source = pixels(&[[1, 2, 3, 255]; 4]);
        let data = buffer_data(wl_shm::Format::Argb8888, 2, 2, 8, 0);
        let mut previous = copy(&source, &data, &[], &mut None, None);

        let changed = pixels(&[[9, 8, 7, 0]; 4]);
        let snapshot = copy(
            &changed,
            &data,
            &[damage(1, 1, 10, 10)],
            &mut previous,
            None,
        )
        .unwrap();
        assert_eq!(
            snapshot.pixels,
            pixels(&[[3, 2, 1, 255], [3, 2, 1, 255], [3, 2, 1, 255], [7, 8, 9, 0],])
        );
        assert!(!snapshot.opaque());

        let mut previous = Some(snapshot);
        let snapshot = copy(&changed, &data, &[damage(5, 5, 1, 1)], &mut previous, None).unwrap();
        assert_eq!(
            snapshot.pixels,
            pixels(&[[3, 2, 1, 255], [3, 2, 1, 255], [3, 2, 1, 255], [7, 8, 9, 0],])
        );
    }
}
