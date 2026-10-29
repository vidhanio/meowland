//! Native-fence DMA-BUF submission and completion-gated CPU readback.

use std::{error::Error as _, ffi::OsStr, fs::OpenOptions, io, os::fd::OwnedFd, time::Duration};

use meowland_core::pixels::FrameSize;
use smithay::{
    backend::{
        allocator::{
            Buffer, Format, Fourcc,
            dmabuf::{Dmabuf, DmabufFlags},
        },
        drm::{DrmNode, NodeType},
        egl::{EGLContext, EGLDevice, EGLDisplay, Error as EglError, fence::EGLFence},
        renderer::{
            Bind, ExportMem, Frame, ImportDma, Offscreen, Renderer, TextureFilter, TextureMapping,
            gles::{
                Capability, GlesError, GlesMapping, GlesRenderbuffer, GlesRenderer, GlesTexture,
            },
        },
    },
    utils::{Rectangle, Transform},
};
use thiserror::Error;

use super::snapshot::{Opacity, Snapshot};

#[derive(Debug, Error)]
enum BackendError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Egl(#[from] EglError),
    #[error(transparent)]
    Gles(#[from] GlesError),

    #[error("DRM render node has no path")]
    MissingNodePath,
    #[error("native EGL fence export is unavailable")]
    MissingFenceSync,
    #[error("DMA-BUF readback requires GLES 3 and EGL synchronization")]
    MissingRendererSync,
    #[error("EGL exposes no sampleable RGB32 DMA-BUF formats")]
    NoFormats,
    #[error("invalid DMA-BUF dimensions")]
    InvalidDimensions,
    #[error("DMA-BUF exceeds snapshot bounds")]
    SnapshotBounds,
    #[error("unsupported DMA-BUF flags")]
    UnsupportedFlags,
    #[error("unsupported DMA-BUF format/modifier")]
    UnsupportedFormat,
    #[error("unexpected RGBA readback length")]
    ReadbackLength,
}

/// DMA-BUF failure recording whether the client buffer can safely be released.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct SnapshotError {
    #[source]
    source: BackendError,
    release_safe: bool,
}

impl SnapshotError {
    fn safe(source: impl Into<BackendError>) -> Self {
        Self {
            source: source.into(),
            release_safe: true,
        }
    }

    /// Whether GPU work is absent or completed, so releasing the client buffer
    /// is safe.
    #[must_use]
    pub const fn release_safe(&self) -> bool {
        self.release_safe
    }
}

/// Submitted GPU work and the native fence that gates its readback.
#[derive(Debug)]
pub struct Submission {
    /// Wait for this native fence to become readable before completing the
    /// readback.
    pub fd: OwnedFd,
    /// Resources retained until the fence signals and completion maps the
    /// pixels.
    pub readback: Readback,
}

/// Pending GPU pixels whose texture, target, and fence remain alive until
/// completion.
pub struct Readback {
    mapping: Result<GlesMapping, BackendError>,
    size: FrameSize,
    force_opaque: bool,
    _texture: GlesTexture,
    _target: GlesRenderbuffer,
    _fence: EGLFence,
}

/// Thread-confined EGL renderer for importing DMA-BUFs and submitting fenced
/// readbacks.
pub struct DmabufBackend {
    renderer: GlesRenderer,
    target: GlesRenderbuffer,
    formats: Vec<Format>,
    device: u64,
}

impl std::fmt::Debug for DmabufBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmabufBackend")
            .field("device", &self.device)
            .field("formats", &self.formats)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Readback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Readback")
            .field("size", &self.size)
            .field("force_opaque", &self.force_opaque)
            .finish_non_exhaustive()
    }
}

impl DmabufBackend {
    /// Discover a usable EGL render device, honoring `MEOWLAND_RENDER_NODE` or
    /// logging fallback.
    pub fn discover() -> Option<Self> {
        let selection = std::env::var_os("MEOWLAND_RENDER_NODE");
        if selection.as_deref() == Some(OsStr::new("off")) {
            return None;
        }
        let requested = match selection.as_deref() {
            Some(os) if os == OsStr::new("auto") => None,
            Some(path) => match DrmNode::from_path(path) {
                Ok(node) if node.ty() == NodeType::Render => Some(node),
                Ok(_) => {
                    tracing::warn!(
                        ?path,
                        "MEOWLAND_RENDER_NODE is not a DRM render node; using shm only"
                    );
                    return None;
                }
                Err(error) => {
                    tracing::warn!(?path, %error, "Cannot select DRM render node; using shm only");
                    return None;
                }
            },
            None => None,
        };
        // SAFETY: This is the trusted system EGL implementation, loaded before
        // EGL use on the compositor thread. Its handle stays alive throughout
        // Smithay's graphics initialization.
        let _egl = match unsafe { libloading::Library::new("libEGL.so.1") } {
            Ok(library) => library,
            Err(error) => {
                tracing::warn!(
                    %error,
                    source = error.source().map(tracing::field::display),
                    "Cannot load EGL; using shm only"
                );
                return None;
            }
        };
        let devices = match EGLDevice::enumerate() {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!(%error, "Cannot enumerate EGL devices; using shm only");
                return None;
            }
        };
        for device in devices {
            let node = match device.try_get_render_node() {
                Ok(Some(node)) if node.ty() == NodeType::Render => node,
                _ => continue,
            };
            if requested.is_some_and(|requested| requested != node) {
                continue;
            }
            match Self::initialize(device, node) {
                Ok(backend) => {
                    tracing::info!(node = ?node.dev_path(), formats = backend.formats.len(), "Enabled asynchronous DMA-BUF readback");
                    return Some(backend);
                }
                Err(error) => {
                    tracing::warn!(node = ?node.dev_path(), %error, "Cannot initialize DMA-BUF readback on EGL device");
                }
            }
        }
        tracing::warn!("No usable EGL render device; using shm only");
        None
    }

    fn initialize(device: EGLDevice, node: DrmNode) -> Result<Self, BackendError> {
        let path = node.dev_path().ok_or(BackendError::MissingNodePath)?;
        let _access = OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: The enumerated device is valid, and Smithay exclusively
        // manages this display's EGL initialization and termination.
        let display = unsafe { EGLDisplay::new(device)? };
        if !display
            .extensions()
            .iter()
            .any(|extension| extension == "EGL_ANDROID_native_fence_sync")
        {
            return Err(BackendError::MissingFenceSync);
        }
        let context = EGLContext::new(&display)?;
        // SAFETY: This fresh context has no external GL state or users and
        // remains exclusively owned by the renderer on the compositor
        // thread.
        let mut renderer = unsafe { GlesRenderer::new(context)? };
        if !renderer.capabilities().contains(&Capability::Fencing)
            || !renderer.capabilities().contains(&Capability::ExportFence)
        {
            return Err(BackendError::MissingRendererSync);
        }
        let formats: Vec<_> = renderer
            .dmabuf_formats()
            .iter()
            .copied()
            .filter(|format| {
                matches!(
                    format.code,
                    Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
                )
            })
            .collect();
        if formats.is_empty() {
            return Err(BackendError::NoFormats);
        }
        renderer.downscale_filter(TextureFilter::Nearest)?;
        renderer.upscale_filter(TextureFilter::Nearest)?;
        let target = renderer.create_buffer(Fourcc::Abgr8888, (1, 1).into())?;
        let mut backend = Self {
            renderer,
            target,
            formats,
            device: node.dev_id(),
        };
        backend.native_fence()?;
        Ok(backend)
    }

    /// DRM device number advertised with the supported DMA-BUF feedback.
    #[must_use]
    pub const fn device(&self) -> u64 {
        self.device
    }

    /// Sampleable RGB32 formats and modifiers supported by this renderer.
    #[must_use]
    pub fn formats(&self) -> &[Format] {
        &self.formats
    }

    fn dimensions(
        &self,
        dmabuf: &Dmabuf,
        bound: Option<(u32, u32)>,
    ) -> Result<FrameSize, SnapshotError> {
        let size = dmabuf.size();
        if size.w <= 0 || size.h <= 0 {
            return Err(SnapshotError::safe(BackendError::InvalidDimensions));
        }
        let width = u32::try_from(size.w)
            .map_err(|_| SnapshotError::safe(BackendError::InvalidDimensions))?;
        let height = u32::try_from(size.h)
            .map_err(|_| SnapshotError::safe(BackendError::InvalidDimensions))?;
        let size = FrameSize::new(width, height)
            .ok_or_else(|| SnapshotError::safe(BackendError::SnapshotBounds))?;
        if !size.fits(bound) {
            return Err(SnapshotError::safe(BackendError::SnapshotBounds));
        }
        if dmabuf.flags().bits() & !DmabufFlags::Y_INVERT.bits() != 0 {
            return Err(SnapshotError::safe(BackendError::UnsupportedFlags));
        }
        if !self.formats.contains(&dmabuf.format()) {
            return Err(SnapshotError::safe(BackendError::UnsupportedFormat));
        }
        Ok(size)
    }

    /// Check bounded dimensions, supported flags and format, and successful GPU
    /// import.
    pub fn validate(&mut self, dmabuf: &Dmabuf) -> bool {
        self.dimensions(dmabuf, None).is_ok() && self.renderer.import_dmabuf(dmabuf, None).is_ok()
    }

    /// The later native fence covers both rendering and readback in this
    /// context.
    fn readback(
        &mut self,
        dmabuf: &Dmabuf,
        texture: &GlesTexture,
    ) -> Result<GlesMapping, BackendError> {
        let size = dmabuf.size();
        let mut framebuffer = self.renderer.bind(&mut self.target)?;
        let destination = Rectangle::from_size((size.w, size.h).into());
        let source = Rectangle::new(
            (
                0.0,
                if dmabuf.y_inverted() {
                    -f64::from(size.h)
                } else {
                    0.0
                },
            )
                .into(),
            (f64::from(size.w), f64::from(size.h)).into(),
        );
        {
            let mut frame = self.renderer.render(
                &mut framebuffer,
                (size.w, size.h).into(),
                Transform::Normal,
            )?;
            Frame::render_texture_from_to(
                &mut frame,
                texture,
                source,
                destination,
                &[destination],
                &[destination],
                Transform::Normal,
                1.0,
            )?;
            drop(Frame::finish(frame)?);
        }
        Ok(self.renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::from_size(size),
            Fourcc::Abgr8888,
        )?)
    }

    /// Flush the current context through a zero-timeout EGL poll before export.
    /// A pending fence is expected; this does not wait for GPU completion.
    fn native_fence(&mut self) -> Result<(EGLFence, OwnedFd), BackendError> {
        let display = self.renderer.egl_context().display().clone();
        Ok(self.renderer.with_context(|_| {
            let fence = EGLFence::create(&display)?;
            fence.client_wait(Some(Duration::ZERO), true)?;
            let fd = fence.export()?;
            Ok::<_, EglError>((fence, fd))
        })??)
    }

    /// Submit rendering and PBO readback in one context, then fence both
    /// operations.
    ///
    /// # Errors
    /// Returns a snapshot error; check `release_safe` before releasing the
    /// client buffer.
    pub fn submit(
        &mut self,
        dmabuf: &Dmabuf,
        bound: Option<(u32, u32)>,
    ) -> Result<Submission, SnapshotError> {
        let size = self.dimensions(dmabuf, bound)?;
        let texture = self
            .renderer
            .import_dmabuf(dmabuf, None)
            .map_err(SnapshotError::safe)?;
        if self.target.size() != dmabuf.size() {
            self.target = self
                .renderer
                .create_buffer(Fourcc::Abgr8888, dmabuf.size())
                .map_err(SnapshotError::safe)?;
        }
        let mapping = self.readback(dmabuf, &texture);
        let (fence, fd) = self.native_fence().map_err(|source| SnapshotError {
            source,
            release_safe: false,
        })?;
        Ok(Submission {
            fd,
            readback: Readback {
                mapping,
                size,
                force_opaque: matches!(dmabuf.format().code, Fourcc::Xrgb8888 | Fourcc::Xbgr8888),
                _texture: texture,
                _target: self.target.clone(),
                _fence: fence,
            },
        })
    }

    /// Map only after the submission's native fence has reported completion.
    ///
    /// # Errors
    /// Returns an error if mapping or pixel conversion fails.
    pub fn complete(&mut self, readback: Readback) -> Result<Snapshot, SnapshotError> {
        let size = readback.size;
        let mapping = readback.mapping.map_err(SnapshotError::safe)?;
        let top_down = mapping.flipped();
        let bytes = self
            .renderer
            .map_texture(&mapping)
            .map_err(SnapshotError::safe)?;
        let row_bytes = size.rgba_stride();
        let needed = size.rgba_len();
        if bytes.len() != needed {
            return Err(SnapshotError::safe(BackendError::ReadbackLength));
        }
        let mut pixels = vec![0; needed];
        if top_down {
            pixels.copy_from_slice(bytes);
        } else {
            for (destination, source) in pixels
                .chunks_exact_mut(row_bytes)
                .zip(bytes.chunks_exact(row_bytes).rev())
            {
                destination.copy_from_slice(source);
            }
        }
        let opacity = if readback.force_opaque {
            Opacity::Force
        } else {
            Opacity::Source
        };
        Ok(Snapshot::from_rgba(
            size.width(),
            size.height(),
            pixels,
            opacity,
        ))
    }
}
