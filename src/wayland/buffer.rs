//! Copy shared-memory and GPU buffers into owned snapshots.

use smithay::{
    backend::allocator::{
        Buffer as _, Format as DmabufFormat, Fourcc, Modifier,
        dmabuf::{
            Dmabuf, DmabufMapping, DmabufMappingFailed, DmabufMappingMode, DmabufSyncFailed,
            DmabufSyncFlags,
        },
    },
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format as ShmFormat},
    wayland::{dmabuf::get_dmabuf, shm::with_buffer_contents},
};

use crate::render::{Image, SourceFormat};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    /// Distance between rows in `pixels`, in bytes.
    pub stride: u32,
    /// The scale the client rendered this buffer at.
    pub scale: i32,
    pub format: SourceFormat,
    pub pixels: Vec<u8>,
}

impl Snapshot {
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
    pub fn logical_size(&self) -> (i32, i32) {
        let scale = self.scale.max(1) as u32;
        (
            (self.width / scale).max(1) as i32,
            (self.height / scale).max(1) as i32,
        )
    }

    pub fn fill(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
        scale: i32,
        format: SourceFormat,
    ) {
        self.pixels.clear();
        self.pixels.extend_from_slice(pixels);
        self.width = width;
        self.height = height;
        self.stride = stride;
        self.scale = scale;
        self.format = format;
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

    /// Allow a buffer up to twice the pane size while a resize catches up.
    fn fits(limit: Option<(u32, u32)>, width: u32, height: u32, scale: i32) -> bool {
        let Some(limit) = limit else {
            return true;
        };
        let scale = scale.max(1) as u32;
        u64::from(width / scale) <= u64::from(limit.0) * 2
            && u64::from(height / scale) <= u64::from(limit.1) * 2
    }

    /// Number of bytes through the last pixel, including spacing between rows.
    fn pixel_extent(width: u32, height: u32, stride: usize) -> Option<usize> {
        let row = (width as usize).checked_mul(4)?;
        if height == 0 || stride < row {
            return None;
        }
        (height as usize - 1).checked_mul(stride)?.checked_add(row)
    }
}

pub const fn shm_format(format: ShmFormat) -> Option<SourceFormat> {
    match format {
        ShmFormat::Argb8888 => Some(SourceFormat::Argb8888),
        ShmFormat::Xrgb8888 => Some(SourceFormat::Xrgb8888),
        _ => None,
    }
}

pub const fn dmabuf_format(format: DmabufFormat) -> Option<SourceFormat> {
    match format.code {
        Fourcc::Argb8888 => Some(SourceFormat::Argb8888),
        Fourcc::Xrgb8888 => Some(SourceFormat::Xrgb8888),
        _ => None,
    }
}

/// Copy a client buffer, bounded by `limit` when a pane shows it.
pub fn snapshot(
    buffer: &WlBuffer,
    scale: i32,
    limit: Option<(u32, u32)>,
    destination: &mut Snapshot,
    gpu: Option<&mut crate::wayland::gpu::Renderer>,
) -> bool {
    if let Some(copied) = copy_dmabuf(buffer, scale, limit, destination, gpu) {
        return copied;
    }
    copy_shm(buffer, scale, limit, destination)
}

/// Check readability before accepting a GPU buffer.
pub fn dmabuf_readable(
    dmabuf: &Dmabuf,
    gpu: Option<&mut crate::wayland::gpu::Renderer>,
) -> Result<(), Unreadable> {
    if dmabuf_format(dmabuf.format()).is_none() {
        return Err(Unreadable::Layout(dmabuf.format()));
    }
    if dmabuf.format().modifier == Modifier::Linear && read_plane(dmabuf, |_, _| ()).is_ok() {
        return Ok(());
    }
    let gpu = gpu.ok_or(Unreadable::NoRenderer)?;
    gpu.can_read(dmabuf).map_err(Unreadable::Renderer)
}

#[derive(Debug)]
pub enum Unreadable {
    /// The client laid the pixels out in a way that cannot be read as rows.
    Layout(DmabufFormat),
    /// More than one plane: only single-plane buffers are read here.
    Planes(usize),
    /// The client did not say how the rows are spaced.
    NoStride,
    /// The row spacing or dimensions cannot describe a valid pixel buffer.
    InvalidGeometry,
    /// The buffer is shorter than the geometry the client described.
    Short {
        claimed: usize,
        mapped: usize,
    },
    Map(DmabufMappingFailed),
    NoRenderer,
    Renderer(crate::Error),
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
            Self::InvalidGeometry => write!(f, "the buffer has invalid dimensions or row spacing"),
            Self::Short { claimed, mapped } => write!(
                f,
                "the buffer claims {claimed} bytes of pixels but maps {mapped}"
            ),
            Self::Map(err) => write!(f, "the buffer could not be mapped: {err}"),
            Self::NoRenderer => write!(f, "there is no renderer to read it through"),
            Self::Renderer(err) => write!(f, "the renderer could not read it: {err}"),
            Self::Sync(err) => write!(f, "the buffer could not be synchronized: {err}"),
        }
    }
}

/// `None` means this buffer is not a DMA buffer.
fn copy_dmabuf(
    buffer: &WlBuffer,
    scale: i32,
    limit: Option<(u32, u32)>,
    destination: &mut Snapshot,
    gpu: Option<&mut crate::wayland::gpu::Renderer>,
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
    if !Snapshot::fits(limit, width, height, scale) {
        tracing::debug!(width, height, ?limit, "refusing an oversized client buffer");
        return Some(false);
    }

    let copy = |pixels: &[u8], stride: u32, destination: &mut Snapshot| {
        destination.fill(pixels, stride, width, height, scale, format);
    };
    let mapped = dmabuf.format().modifier == Modifier::Linear
        && read_plane(dmabuf, |pixels, stride| copy(pixels, stride, destination)).is_ok();
    if mapped {
        tracing::debug!(width, height, ?format, scale, "mapped a client buffer");
        return Some(true);
    }

    let Some(gpu) = gpu else {
        return Some(false);
    };
    match gpu.read(dmabuf, format, scale, destination) {
        Ok(()) => {
            tracing::debug!(width, height, ?format, scale, "read a client buffer back");
            Some(true)
        }
        Err(err) => {
            tracing::debug!(?err, ?format, "could not read a client's GPU buffer");
            Some(false)
        }
    }
}

fn copy_shm(
    buffer: &WlBuffer,
    scale: i32,
    limit: Option<(u32, u32)>,
    destination: &mut Snapshot,
) -> bool {
    let copied = with_buffer_contents::<_, bool>(buffer, |pointer, length, data| {
        if data.offset < 0 || data.width <= 0 || data.height <= 0 || data.stride <= 0 {
            return false;
        }
        let Some(format) = shm_format(data.format) else {
            return false;
        };
        let (width, height, stride) = (data.width as u32, data.height as u32, data.stride as usize);
        if !Snapshot::fits(limit, width, height, scale) {
            tracing::debug!(width, height, ?limit, "refusing an oversized client buffer");
            return false;
        }
        let Some(last) = Snapshot::pixel_extent(width, height, stride) else {
            return false;
        };
        let Some(end) = (data.offset as usize).checked_add(last) else {
            return false;
        };
        if end > length {
            return false;
        }
        // SAFETY: the pointer is valid for `length` bytes for as long as this
        // closure runs, which is the contract of `with_buffer_contents`. The
        // region was checked against that length, and the slice does not
        // outlive the copy below. A client that writes at the same time can
        // only cause a torn copy.
        #[expect(
            unsafe_code,
            reason = "shared memory is only reachable as a raw pointer; the slice is bounded and is copied out at once"
        )]
        let source = unsafe { std::slice::from_raw_parts(pointer.add(data.offset as usize), last) };
        destination.fill(source, stride as u32, width, height, scale, format);
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

/// Map a plane within a CPU access synchronization bracket.
fn read_plane<T>(dmabuf: &Dmabuf, read: impl FnOnce(&[u8], u32) -> T) -> Result<T, Unreadable> {
    if dmabuf.num_planes() != 1 {
        return Err(Unreadable::Planes(dmabuf.num_planes()));
    }
    let stride = dmabuf.strides().next().ok_or(Unreadable::NoStride)?;
    let (width, height) = (dmabuf.width(), dmabuf.height());
    let last = Snapshot::pixel_extent(width, height, stride as usize)
        .ok_or(Unreadable::InvalidGeometry)?;

    let plane = MappedPlane::new(dmabuf)?;
    if last > plane.mapping.length() {
        return Err(Unreadable::Short {
            claimed: last,
            mapped: plane.mapping.length(),
        });
    }
    // SAFETY: the mapping is valid for `plane.mapping.length()` bytes until it
    // is dropped, `last` was checked against that length a line ago, and the
    // slice does not outlive the mapping. A client that writes at the same time
    // can only cause a torn copy.
    #[expect(
        unsafe_code,
        reason = "a mapped buffer is only reachable as a raw pointer; the slice is bounded by the mapping and is copied out at once"
    )]
    let pixels = unsafe { std::slice::from_raw_parts(plane.mapping.ptr().cast::<u8>(), last) };
    Ok(read(pixels, stride))
}

/// A mapped plane and the synchronization bracket that makes it readable.
///
/// Field order is significant: Rust drops fields in declaration order, so the
/// bracket closes while the mapping is still valid, including during an unwind.
#[derive(Debug)]
struct MappedPlane<'a> {
    _reading: PlaneRead<'a>,
    mapping: DmabufMapping,
}

impl<'a> MappedPlane<'a> {
    fn new(dmabuf: &'a Dmabuf) -> Result<Self, Unreadable> {
        let reading = PlaneRead::start(dmabuf).map_err(Unreadable::Sync)?;
        let mapping = dmabuf
            .map_plane(0, DmabufMappingMode::READ)
            .map_err(Unreadable::Map)?;
        Ok(Self {
            _reading: reading,
            mapping,
        })
    }
}

#[derive(Debug)]
struct PlaneRead<'a>(&'a Dmabuf);

impl<'a> PlaneRead<'a> {
    fn start(dmabuf: &'a Dmabuf) -> Result<Self, DmabufSyncFailed> {
        dmabuf.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)?;
        Ok(Self(dmabuf))
    }
}

impl Drop for PlaneRead<'_> {
    fn drop(&mut self) {
        if let Err(err) = self
            .0
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

        // The modifier decides how the pixels are reached, not whether they can
        // be reached. A tiled buffer is read through the renderer instead of
        // being mapped, and its color order is the same either way.
        let unstated = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Invalid,
        };
        let tiled = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Unrecognized(0x42),
        };
        assert_eq!(dmabuf_format(unstated), Some(SourceFormat::Argb8888));
        assert_eq!(dmabuf_format(tiled), Some(SourceFormat::Argb8888));

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

    #[test]
    fn pixel_extent_rejects_invalid_rows() {
        assert_eq!(Snapshot::pixel_extent(2, 3, 12), Some(32));
        assert_eq!(Snapshot::pixel_extent(2, 3, 7), None);
        assert_eq!(Snapshot::pixel_extent(2, 0, 8), None);
        assert_eq!(Snapshot::pixel_extent(1, 3, usize::MAX), None);
    }

    #[test]
    fn pane_limit_does_not_overflow_for_large_screens() {
        assert!(Snapshot::fits(
            Some((u32::MAX, u32::MAX)),
            u32::MAX,
            u32::MAX,
            1
        ));
    }
}
