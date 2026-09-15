//! Reading client pixels out of `wl_buffer`s.
//!
//! Clients hand the compositor pixels in one of two ways: shared memory
//! (`wl_shm`), or a file descriptor for memory a GPU driver allocated
//! (`zwp_linux_dmabuf_v1`). Either way the memory is theirs, and they are
//! allowed to reuse it as soon as they are told the compositor is done with it.
//! So the compositor copies it out immediately ([`snapshot`]) and hands the
//! buffer straight back - after that the pixels are ours, and compositing never
//! touches client memory again.
//!
//! What the two ways have in common is the copy; what they do not is what makes
//! it safe. Shared memory arrives with a length, and the copy is bounded by it.
//! A GPU buffer arrives as a file descriptor that has to be mapped, read inside
//! a synchronization bracket, and unmapped again - which is also where the
//! layout matters: only a linear buffer can be read without the driver that
//! wrote it.
//!
//! That copy is the reason this module exists, and why it holds the only
//! `unsafe` in the crate (see `Cargo.toml` for the lint exception).

use smithay::{
    backend::allocator::{
        Buffer as _, Format as DmabufFormat, Fourcc, Modifier,
        dmabuf::{
            Dmabuf, DmabufMappingFailed, DmabufMappingMode, DmabufSyncFailed, DmabufSyncFlags,
        },
    },
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format as ShmFormat},
    wayland::{dmabuf::get_dmabuf, shm::with_buffer_contents},
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
    /// Empty storage used until a surface commits its first valid buffer.
    pub const fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            scale: 1,
            format: SourceFormat::Xrgb8888,
            pixels: Vec::new(),
        }
    }
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
pub const fn shm_format(format: ShmFormat) -> Option<SourceFormat> {
    match format {
        ShmFormat::Argb8888 => Some(SourceFormat::Argb8888),
        ShmFormat::Xrgb8888 => Some(SourceFormat::Xrgb8888),
        _ => None,
    }
}

/// Translate a GPU buffer's layout into the one the renderer understands.
///
/// Linear is the only layout a mapping can be interpreted without the driver
/// that allocated it: rows of pixels, one after another. Anything else - a
/// tiled or compressed layout, or a modifier the client did not state - would
/// be read as rows and painted as noise.
pub const fn dmabuf_format(format: DmabufFormat) -> Option<SourceFormat> {
    if !matches!(format.modifier, Modifier::Linear) {
        return None;
    }
    match format.code {
        Fourcc::Argb8888 => Some(SourceFormat::Argb8888),
        Fourcc::Xrgb8888 => Some(SourceFormat::Xrgb8888),
        _ => None,
    }
}

/// Copy out the pixels of a client buffer, whichever kind it is.
///
/// `limit` is the size of the screen the copy is meant for: a buffer far larger
/// than that is refused rather than copied, so a client cannot ask the
/// compositor to hold memory it has no screen for.
///
/// Returns `false` for buffers that come from a protocol meowland does not
/// advertise, for formats it cannot composite, and for buffers whose advertised
/// geometry does not fit the memory the client handed over.
pub fn snapshot(
    buffer: &WlBuffer,
    scale: i32,
    limit: (u32, u32),
    destination: &mut Snapshot,
) -> bool {
    if let Some(copied) = copy_dmabuf(buffer, scale, limit, destination) {
        return copied;
    }
    copy_shm(buffer, scale, limit, destination)
}

/// Whether a GPU buffer is one the compositor will be able to read.
///
/// This is what decides whether a client is told its buffer is good, and it is
/// asked before the client draws into it, so that a buffer meowland could not
/// composite is refused while the client can still fall back to shared memory.
pub fn dmabuf_readable(dmabuf: &Dmabuf) -> Result<(), Unreadable> {
    if dmabuf_format(dmabuf.format()).is_none() {
        return Err(Unreadable::Layout(dmabuf.format()));
    }
    read_plane(dmabuf, |_, _| ())
}

/// Why a GPU buffer cannot be read.
///
/// Each of these is a different thing to go and look at when a client's window
/// comes up blank, so they are told apart rather than collapsed into "no".
#[derive(Debug)]
pub enum Unreadable {
    /// The client laid the pixels out in a way that cannot be read as rows.
    Layout(DmabufFormat),
    /// More than one plane: only single-plane buffers are read here.
    Planes(usize),
    /// The client did not say how the rows are spaced.
    NoStride,
    /// The buffer is shorter than the geometry the client described.
    Short { claimed: usize, mapped: usize },
    /// The buffer could not be mapped into this process.
    Map(DmabufMappingFailed),
    /// Reading the buffer could not be bracketed for the driver.
    Sync(DmabufSyncFailed),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(format) => {
                write!(f, "the layout {format:?} cannot be read as rows of pixels")
            }
            Self::Planes(count) => write!(f, "the buffer has {count} planes, not one"),
            Self::NoStride => write!(f, "the buffer has no row spacing"),
            Self::Short { claimed, mapped } => write!(
                f,
                "the buffer claims {claimed} bytes of pixels but maps {mapped}"
            ),
            Self::Map(err) => write!(f, "the buffer could not be mapped: {err}"),
            Self::Sync(err) => write!(f, "the buffer could not be synchronized: {err}"),
        }
    }
}

/// Whether a buffer is small enough to be worth copying.
///
/// Twice the screen is generous room for a window that grew before the
/// compositor caught up; past that the client is asking for memory, not for
/// pixels.
fn fits(limit: (u32, u32), width: u32, height: u32, scale: i32) -> bool {
    let scale = scale.max(1) as u32;
    width / scale <= limit.0 * 2 && height / scale <= limit.1 * 2
}

/// Copy out the pixels of a GPU buffer.
///
/// `None` when the buffer is not one: a `wl_shm` buffer, or one from a protocol
/// that is not advertised. Otherwise `Some` says whether it was copied, so that
/// a GPU buffer the compositor refuses does not fall through to the shared
/// memory reader and get reported as the wrong kind of failure.
fn copy_dmabuf(
    buffer: &WlBuffer,
    scale: i32,
    limit: (u32, u32),
    destination: &mut Snapshot,
) -> Option<bool> {
    let dmabuf = get_dmabuf(buffer).ok()?;
    let Some(format) = dmabuf_format(dmabuf.format()) else {
        tracing::debug!(
            format = ?dmabuf.format(),
            "refusing a GPU buffer whose layout meowland cannot read"
        );
        return Some(false);
    };
    let (width, height) = (dmabuf.width(), dmabuf.height());
    if !fits(limit, width, height, scale) {
        tracing::debug!(width, height, ?limit, "refusing an oversized client buffer");
        return Some(false);
    }
    let copied = match read_plane(dmabuf, |pixels, stride| {
        destination.pixels.clear();
        destination.pixels.extend_from_slice(pixels);
        destination.width = width;
        destination.height = height;
        destination.stride = stride;
        destination.scale = scale;
        destination.format = format;
    }) {
        Ok(()) => {
            tracing::debug!(
                width,
                height,
                stride = destination.stride,
                ?format,
                scale,
                "copied a client buffer from the GPU"
            );
            true
        }
        Err(reason) => {
            tracing::debug!(reason = %reason, ?format, "could not read a client's GPU buffer");
            false
        }
    };
    Some(copied)
}

/// Copy out the pixels of a shared memory buffer.
fn copy_shm(buffer: &WlBuffer, scale: i32, limit: (u32, u32), destination: &mut Snapshot) -> bool {
    let copied = with_buffer_contents::<_, bool>(buffer, |pointer, length, data| {
        if data.offset < 0 || data.width <= 0 || data.height <= 0 || data.stride <= 0 {
            return false;
        }
        let Some(format) = shm_format(data.format) else {
            return false;
        };
        let (width, height, stride) = (data.width as u32, data.height as u32, data.stride as usize);
        if !fits(limit, width, height, scale) {
            tracing::debug!(width, height, ?limit, "refusing an oversized client buffer");
            return false;
        }
        // The rows are only required to be `stride` apart, so the last byte we
        // read decides whether the client's advertisement is consistent
        // with the pool it gave us.
        let last = (height as usize - 1) * stride + width as usize * 4;
        if data.offset as usize + last > length {
            return false;
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
        destination.pixels.clear();
        destination.pixels.extend_from_slice(source);
        destination.width = width;
        destination.height = height;
        destination.stride = stride as u32;
        destination.scale = scale;
        destination.format = format;
        tracing::debug!(
            width,
            height,
            stride,
            ?format,
            scale,
            "copied a client buffer"
        );
        true
    });
    match copied {
        Ok(copied) => copied,
        Err(err) => {
            tracing::debug!(?err, "could not copy a client buffer");
            false
        }
    }
}

/// Read the single plane of a GPU buffer.
///
/// The plane is mapped, handed to `read` as rows of pixels, and unmapped again.
/// The mapping is only readable between the two halves of a synchronization
/// bracket, and the lengths involved are the client's word against the memory
/// it handed over, so both are checked here rather than at the call sites.
///
/// The bracket comes first on purpose: a driver is entitled to keep the buffer
/// somewhere the CPU cannot reach - that is what video memory is - and
/// beginning CPU access is how it is told to put it somewhere the CPU can.
/// A buffer mapped without that is refused, which is exactly what happens when
/// the order is the other way round.
fn read_plane<T>(dmabuf: &Dmabuf, read: impl FnOnce(&[u8], u32) -> T) -> Result<T, Unreadable> {
    if dmabuf.num_planes() != 1 {
        return Err(Unreadable::Planes(dmabuf.num_planes()));
    }
    let stride = dmabuf.strides().next().ok_or(Unreadable::NoStride)?;
    let (width, height) = (dmabuf.width(), dmabuf.height());
    let rows = (height.saturating_sub(1) as usize).saturating_mul(stride as usize);
    let last = rows.saturating_add(width as usize * 4);

    // The bracket has to be held until the mapping is gone, which is what
    // dropping in reverse order below does.
    let reading = PlaneRead::start(dmabuf).map_err(Unreadable::Sync)?;
    let mapping = dmabuf
        .map_plane(0, DmabufMappingMode::READ)
        .map_err(Unreadable::Map)?;
    if last > mapping.length() {
        return Err(Unreadable::Short {
            claimed: last,
            mapped: mapping.length(),
        });
    }
    // SAFETY: the mapping is valid for `mapping.length()` bytes until it is
    // dropped, `last` was just checked against that length, and the slice does
    // not outlive the mapping. A client writing concurrently - without the
    // synchronization bracket this is inside - can only cost us a torn copy.
    #[expect(
        unsafe_code,
        reason = "a mapped buffer is only reachable as a raw pointer; the slice is bounded by the mapping and is copied out at once"
    )]
    let pixels = unsafe { std::slice::from_raw_parts(mapping.ptr().cast::<u8>(), last) };
    let read = read(pixels, stride);
    // Order matters: the bracket has to close while the mapping is still there.
    drop(reading);
    drop(mapping);
    Ok(read)
}

/// A mapped plane, bracketed for reading.
///
/// Memory a client's GPU has just written is not necessarily the memory its CPU
/// would read; the bracket is how the compositor asks the driver for a
/// consistent view. Guarding it keeps the two halves paired, including on the
/// paths that give up early.
#[derive(Debug)]
struct PlaneRead<'a> {
    dmabuf: &'a Dmabuf,
}

impl<'a> PlaneRead<'a> {
    fn start(dmabuf: &'a Dmabuf) -> Result<Self, DmabufSyncFailed> {
        dmabuf.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)?;
        Ok(Self { dmabuf })
    }
}

impl Drop for PlaneRead<'_> {
    fn drop(&mut self) {
        if let Err(err) = self
            .dmabuf
            .sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ)
        {
            tracing::warn!(?err, "could not finish reading a client's GPU buffer");
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::{Format as DmabufFormat, Fourcc, Modifier};

    use super::*;

    #[test]
    fn formats_we_cannot_composite_are_rejected() {
        assert_eq!(
            shm_format(ShmFormat::Xrgb8888),
            Some(SourceFormat::Xrgb8888)
        );
        assert_eq!(
            shm_format(ShmFormat::Argb8888),
            Some(SourceFormat::Argb8888)
        );
        assert_eq!(shm_format(ShmFormat::Rgb565), None);
    }

    #[test]
    fn gpu_buffers_are_only_read_when_their_layout_is_known() {
        let argb = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        };
        let xrgb = DmabufFormat {
            code: Fourcc::Xrgb8888,
            modifier: Modifier::Linear,
        };
        assert_eq!(dmabuf_format(argb), Some(SourceFormat::Argb8888));
        assert_eq!(dmabuf_format(xrgb), Some(SourceFormat::Xrgb8888));

        // A modifier the client did not state, and one it did: neither can be
        // read as rows of pixels.
        let unstated = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Invalid,
        };
        let tiled = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Unrecognized(0x42),
        };
        assert_eq!(dmabuf_format(unstated), None);
        assert_eq!(dmabuf_format(tiled), None);

        let rgb565 = DmabufFormat {
            code: Fourcc::Rgb565,
            modifier: Modifier::Linear,
        };
        assert_eq!(dmabuf_format(rgb565), None);
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
