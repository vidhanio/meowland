use std::{ffi::OsStr, fs::OpenOptions, io};

use smithay::{
    backend::{
        allocator::{
            Buffer, Format, Fourcc,
            dmabuf::{Dmabuf, DmabufFlags},
        },
        drm::{DrmNode, NodeType},
        egl::{EGLContext, EGLDevice, EGLDisplay, Error as EglError, fence::EGLFence},
        renderer::{
            Bind, Color32F, ExportMem, Frame, ImportDma, Offscreen, Renderer, TextureFilter,
            TextureMapping,
            gles::{Capability, GlesError, GlesRenderbuffer, GlesRenderer},
            sync::Interrupted,
        },
    },
    utils::{Rectangle, Transform},
};
use thiserror::Error;

use super::{MAX_SURFACE_PIXELS, MAX_SURFACE_SIDE, snapshot::Snapshot};

#[derive(Debug, Error)]
enum BackendError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Egl(#[from] EglError),
    #[error(transparent)]
    Gles(#[from] GlesError),
    #[error(transparent)]
    Wait(#[from] Interrupted),
    #[error("DRM render node has no path")]
    MissingNodePath,
    #[error("EGL fence synchronization is unavailable")]
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

#[derive(Debug, Error)]
#[error("{source}")]
pub(super) struct SnapshotError {
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

    pub(super) const fn release_safe(&self) -> bool {
        self.release_safe
    }
}

pub(super) struct DmabufBackend {
    renderer: GlesRenderer,
    target: GlesRenderbuffer,
    formats: Vec<Format>,
    device: u64,
}

impl DmabufBackend {
    pub(super) fn discover() -> Option<Self> {
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
        #[expect(
            unsafe_code,
            reason = "loading the installed graphics library before Smithay's infallible loader"
        )]
        // SAFETY: EGL supports dynamic loading on this thread. Keep the library
        // alive until Smithay's infallible lazy loader acquires its own reference.
        let _egl = match unsafe { libloading::Library::new("libEGL.so.1") } {
            Ok(library) => library,
            Err(error) => {
                tracing::warn!(%error, "Cannot load EGL; using shm only");
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
                    tracing::info!(node = ?node.dev_path(), formats = backend.formats.len(), "Enabled DMA-BUF import and CPU readback");
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

    #[expect(
        unsafe_code,
        reason = "Smithay owns the EGL display; the fresh context is never shared or used on another thread"
    )]
    fn initialize(device: EGLDevice, node: DrmNode) -> Result<Self, BackendError> {
        let path = node.dev_path().ok_or(BackendError::MissingNodePath)?;
        let _access = OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: Only Smithay creates/terminates this display. The new context
        // is transferred to one renderer and is never active on another thread.
        let display = unsafe { EGLDisplay::new(device)? };
        if !display
            .extensions()
            .iter()
            .any(|extension| extension == "EGL_KHR_fence_sync")
        {
            return Err(BackendError::MissingFenceSync);
        }
        let context = EGLContext::new(&display)?;
        // SAFETY: The freshly created context has not been made current
        // elsewhere.
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
        let mut target: GlesRenderbuffer =
            renderer.create_buffer(Fourcc::Abgr8888, (1, 1).into())?;
        {
            let mut framebuffer = renderer.bind(&mut target)?;
            let completion = {
                let mut frame =
                    renderer.render(&mut framebuffer, (1, 1).into(), Transform::Normal)?;
                frame.clear(
                    Color32F::new(0.0, 0.0, 0.0, 0.0),
                    &[Rectangle::from_size((1, 1).into())],
                )?;
                Frame::finish(frame)?
            };
            completion.wait()?;
            let mapping = renderer.copy_framebuffer(
                &framebuffer,
                Rectangle::from_size((1, 1).into()),
                Fourcc::Abgr8888,
            )?;
            drop(framebuffer);
            renderer.map_texture(&mapping)?;
        }
        Ok(Self {
            renderer,
            target,
            formats,
            device: node.dev_id(),
        })
    }

    pub(super) const fn device(&self) -> u64 {
        self.device
    }

    pub(super) fn formats(&self) -> &[Format] {
        &self.formats
    }

    fn dimensions(
        &self,
        dmabuf: &Dmabuf,
        bound: Option<(u32, u32)>,
    ) -> Result<(u32, u32), SnapshotError> {
        let size = dmabuf.size();
        if size.w <= 0 || size.h <= 0 {
            return Err(SnapshotError::safe(BackendError::InvalidDimensions));
        }
        let (width, height) = (size.w as u32, size.h as u32);
        if width > MAX_SURFACE_SIDE
            || height > MAX_SURFACE_SIDE
            || width as usize * height as usize > MAX_SURFACE_PIXELS
            || bound.is_some_and(|(max_width, max_height)| width > max_width || height > max_height)
        {
            return Err(SnapshotError::safe(BackendError::SnapshotBounds));
        }
        if dmabuf.flags().bits() & !DmabufFlags::Y_INVERT.bits() != 0 {
            return Err(SnapshotError::safe(BackendError::UnsupportedFlags));
        }
        if !self.formats.contains(&dmabuf.format()) {
            return Err(SnapshotError::safe(BackendError::UnsupportedFormat));
        }
        Ok((width, height))
    }

    pub(super) fn validate(&mut self, dmabuf: &Dmabuf) -> bool {
        self.dimensions(dmabuf, None).is_ok() && self.renderer.import_dmabuf(dmabuf, None).is_ok()
    }

    fn readback(
        &mut self,
        dmabuf: &Dmabuf,
    ) -> Result<<GlesRenderer as ExportMem>::TextureMapping, SnapshotError> {
        let size = dmabuf.size();
        let texture = self
            .renderer
            .import_dmabuf(dmabuf, None)
            .map_err(SnapshotError::safe)?;
        if self.target.size() != size {
            self.target = self
                .renderer
                .create_buffer(Fourcc::Abgr8888, size)
                .map_err(SnapshotError::safe)?;
        }
        let mut framebuffer = self
            .renderer
            .bind(&mut self.target)
            .map_err(SnapshotError::safe)?;
        let destination = Rectangle::from_size((size.w, size.h).into());
        // Mark the entire copy opaque to disable blending, not to discard
        // alpha: the shader copies the client's already-premultiplied
        // RGBA unchanged. Smithay negates Y_INVERT texture coordinates
        // without translating them; the negative source origin supplies
        // that translation, yielding 1 - y.
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
        let (drawn, finished) = {
            let mut frame = self
                .renderer
                .render(&mut framebuffer, (size.w, size.h).into(), Transform::Normal)
                .map_err(SnapshotError::safe)?;
            let drawn = Frame::render_texture_from_to(
                &mut frame,
                &texture,
                source,
                destination,
                &[destination],
                &[destination],
                Transform::Normal,
                1.0,
            );
            (drawn, Frame::finish(frame))
        };
        let completion = match finished {
            Ok(sync) => sync.wait().map_err(BackendError::from),
            Err(error) => Err(BackendError::from(error)),
        };
        if let Err(error) = completion {
            let display = self.renderer.egl_context().display().clone();
            let drained = self
                .renderer
                .with_context(|_| EGLFence::create(&display)?.client_wait(None, true));
            if !matches!(drained, Ok(Ok(true))) {
                return Err(SnapshotError {
                    source: error,
                    release_safe: false,
                });
            }
            return Err(SnapshotError::safe(error));
        }
        drawn.map_err(SnapshotError::safe)?;
        let mapping = self
            .renderer
            .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Abgr8888)
            .map_err(SnapshotError::safe)?;
        drop(framebuffer);
        Ok(mapping)
    }

    /// The caller gates implicit writer fences before this synchronous read.
    /// An unsynchronized error must not be followed by a buffer release.
    pub(super) fn snapshot(
        &mut self,
        dmabuf: &Dmabuf,
        bound: Option<(u32, u32)>,
        pixels: &mut Vec<u8>,
    ) -> Result<Snapshot, SnapshotError> {
        let (width, height) = self.dimensions(dmabuf, bound)?;
        let mapping = self.readback(dmabuf)?;
        let top_down = mapping.flipped();
        // Mapping the PBO blocks until readback completes. Nothing below this
        // point borrows the client's storage or can return a partial snapshot.
        let bytes = self
            .renderer
            .map_texture(&mapping)
            .map_err(SnapshotError::safe)?;
        let row_bytes = width as usize * 4;
        let needed = row_bytes * height as usize;
        if bytes.len() != needed {
            return Err(SnapshotError::safe(BackendError::ReadbackLength));
        }
        pixels.resize(needed, 0);
        if pixels.capacity() >= needed.saturating_mul(4).max(1 << 20) {
            pixels.shrink_to_fit();
        }
        // GLES reports flipped mappings because its normal projection puts
        // surface row zero at GL row zero. A non-flipped mapping is bottom-up.
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
        let force_opaque = matches!(dmabuf.format().code, Fourcc::Xrgb8888 | Fourcc::Xbgr8888);
        let mut opaque = true;
        for pixel in pixels.as_chunks_mut::<4>().0 {
            if force_opaque {
                pixel[3] = 255;
            } else {
                opaque &= pixel[3] == 255;
            }
        }
        Ok(Snapshot {
            width,
            height,
            pixels: std::mem::take(pixels),
            opaque,
        })
    }
}
