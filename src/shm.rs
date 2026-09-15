//! Reading client pixels out of `wl_shm` buffers.
//!
//! Clients hand the compositor shared memory, never a slice: the memory is
//! theirs, and they are allowed to reuse it as soon as they are told the
//! compositor is done with it. So the compositor copies it out immediately
//! ([`snapshot`]) and hands the buffer straight back - after that the
//! pixels are ours, and compositing never touches client memory again.
//!
//! That copy is the reason this module exists, and why it holds the only
//! `unsafe` in the crate (see `Cargo.toml` for the lint exception).

use smithay::{
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format},
    wayland::shm::with_buffer_contents,
};

use crate::render::{Image, SourceFormat};

/// A copy of one client buffer, owned by the compositor.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    /// Distance between rows in `pixels`.
    pub stride: u32,
    /// The scale the client rendered this buffer at.
    pub scale: i32,
    pub format: SourceFormat,
    pub pixels: Vec<u8>,
}

impl Snapshot {
    /// The size the client laid this buffer out against, in logical
    /// coordinates.
    pub fn logical_size(&self) -> (i32, i32) {
        let scale = self.scale.max(1) as u32;
        (
            (self.width / scale).max(1) as i32,
            (self.height / scale).max(1) as i32,
        )
    }

    pub fn image(&self) -> Image<'_> {
        Image {
            pixels: &self.pixels,
            stride: self.stride as usize,
            width: self.width,
            height: self.height,
            format: self.format,
        }
    }
}

/// Translate a `wl_shm` format into the layout the renderer understands.
pub const fn source_format(format: Format) -> Option<SourceFormat> {
    match format {
        Format::Argb8888 => Some(SourceFormat::Argb8888),
        Format::Xrgb8888 => Some(SourceFormat::Xrgb8888),
        _ => None,
    }
}

/// Copy out the pixels of a client buffer.
///
/// `limit` is the size of the screen the copy is meant for: a buffer far larger
/// than that is refused rather than copied, so a client cannot ask the
/// compositor to hold memory it has no screen for.
///
/// Returns `None` for buffers that are not shared memory (a client using a
/// protocol meowland does not advertise), for formats it cannot composite, and
/// for buffers whose advertised geometry does not fit into the pool the client
/// handed over.
pub fn snapshot(buffer: &WlBuffer, scale: i32, limit: (u32, u32)) -> Option<Snapshot> {
    let copied = with_buffer_contents::<_, Option<Snapshot>>(buffer, |pointer, length, data| {
        if data.offset < 0 || data.width <= 0 || data.height <= 0 || data.stride <= 0 {
            return None;
        }
        let format = source_format(data.format)?;
        let (width, height, stride) = (data.width as u32, data.height as u32, data.stride as usize);
        // Twice the screen is generous room for a window that grew before the
        // compositor caught up; past that the client is asking for
        // memory, not for pixels.
        let scale_factor = scale.max(1) as u32;
        if width / scale_factor > limit.0 * 2 || height / scale_factor > limit.1 * 2 {
            tracing::debug!(width, height, ?limit, "refusing an oversized client buffer");
            return None;
        }
        // The rows are only required to be `stride` apart, so the last byte we
        // read decides whether the client's advertisement is consistent
        // with the pool it gave us.
        let last = (height as usize - 1) * stride + width as usize * 4;
        if data.offset as usize + last > length {
            return None;
        }
        // SAFETY: the pointer is valid for `length` bytes for the duration of
        // this closure (the contract of `with_buffer_contents`), we
        // bound-checked the region, and the slice does not outlive the
        // copy below. A client writing concurrently can only cost us a torn
        // copy.
        #[expect(
            unsafe_code,
            reason = "shared memory is only reachable as a raw pointer; the slice is bounded and is copied out at once"
        )]
        let source = unsafe { std::slice::from_raw_parts(pointer.add(data.offset as usize), last) };
        let pixels = source.to_vec();
        let non_zero = pixels.iter().filter(|byte| **byte != 0).count();
        tracing::debug!(
            width,
            height,
            stride,
            ?format,
            scale,
            non_zero,
            "copied a client buffer"
        );
        Some(Snapshot {
            width,
            height,
            stride: stride as u32,
            scale,
            format,
            pixels,
        })
    });
    match copied {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(?err, "could not copy a client buffer");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_we_cannot_composite_are_rejected() {
        assert_eq!(
            source_format(Format::Xrgb8888),
            Some(SourceFormat::Xrgb8888)
        );
        assert_eq!(
            source_format(Format::Argb8888),
            Some(SourceFormat::Argb8888)
        );
        assert_eq!(source_format(Format::Rgb565), None);
    }

    #[test]
    fn logical_size_follows_the_buffer_scale() {
        let snapshot = Snapshot {
            width: 200,
            height: 100,
            stride: 800,
            scale: 2,
            format: SourceFormat::Xrgb8888,
            pixels: vec![0; 200 * 100 * 4],
        };
        assert_eq!(snapshot.logical_size(), (100, 50));
        assert_eq!(snapshot.image().stride, 800);
    }
}
