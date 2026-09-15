//! Reading a client's GPU buffer through the renderer, for the buffers no
//! mapping can reach.
//!
//! A driver is entitled to keep a buffer somewhere the CPU cannot reach - that
//! is what video memory is - and asking does not change it: mapping such a
//! buffer fails, and so does reading its descriptor ([`crate::dmabuf`] has the
//! details). The pixels are not unreachable, though. They are unreachable *as
//! memory*: the device that wrote them can still read them, so the way in is to
//! have the GPU draw the buffer somewhere readable and copy that out.
//!
//! So: import the client's buffer as a texture, draw it into an offscreen
//! texture, read that back with the renderer's `ExportMem`, and hand the pixels
//! to [`crate::buffer`], which cannot tell the difference between them and a
//! buffer that was mapped.
//!
//! None of this is needed for a buffer that can be mapped, which is cheaper. It
//! is the answer for the ones that cannot be, which is what a client rendering
//! into video memory hands over - and having an answer is what lets clients be
//! offered GPU buffers at all.

use std::path::{Path, PathBuf};

use smithay::{
    backend::{
        allocator::{Buffer as _, Format, Fourcc, Modifier, dmabuf::Dmabuf},
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind as _, Color32F, ExportMem as _, Frame, ImportDma as _, Offscreen as _,
            Renderer as _, Texture as _,
            gles::{GlesError, GlesRenderer, GlesTexture},
        },
    },
    utils::{Buffer as BufferCoords, Physical, Rectangle, Size, Transform},
};

use crate::render::SourceFormat;

/// The layout a readback is asked for: bytes of red, green, blue and alpha, in
/// that order, which is what `GL_RGBA` means and what almost every driver can
/// write out. Asking for the client's own layout instead would mean asking for
/// `GL_BGRA`, which is an extension.
const READBACK: Fourcc = Fourcc::Abgr8888;

/// The layout a readback comes back in, given the layout the client's buffer
/// was in.
///
/// The channels are in the order [`READBACK`] writes them. The fourth byte is
/// the client's: a layout with no alpha in it stays without one, whichever way
/// round its channels are.
const fn readback_of(format: SourceFormat) -> SourceFormat {
    if format.opaque() {
        SourceFormat::Xbgr8888
    } else {
        SourceFormat::Abgr8888
    }
}

/// Why a buffer could not be brought back through the renderer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The device could not be brought up as a renderer.
    ///
    /// Note that a machine with no EGL at all does not get this far: the
    /// bindings load the library on first use and panic if they cannot, which
    /// is their behaviour and not something this can turn into an error.
    #[error("no renderer on {path}")]
    Device {
        path: PathBuf,
        #[source]
        source: smithay::backend::egl::Error,
    },
    /// The device could not be made into a renderer.
    #[error("no renderer on {path}")]
    Create {
        path: PathBuf,
        #[source]
        source: GlesError,
    },
    /// The client's buffer could not be imported as a texture.
    #[error("the buffer could not be imported")]
    Import(#[source] GlesError),
    /// The client's buffer could not be drawn into somewhere readable.
    #[error("the buffer could not be drawn")]
    Draw(#[source] GlesError),
    /// The drawn buffer could not be read back.
    #[error("the buffer could not be read back")]
    Read(#[source] GlesError),
    /// The renderer's work could not be waited for.
    #[error("the drawing could not be waited for")]
    Sync(#[source] smithay::backend::renderer::sync::Interrupted),
    /// The readback is shorter than the pixels it was asked for.
    #[error("the readback is {actual} bytes, not the {expected} it was asked for")]
    Short { expected: usize, actual: usize },
}

/// An offscreen renderer: everything it draws is drawn to be read back.
pub struct Renderer {
    renderer: GlesRenderer,
    /// The texture buffers are drawn into. Kept between reads so that a client
    /// drawing at a steady size does not make the renderer allocate one per
    /// frame.
    staging: Option<GlesTexture>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("formats", &self.renderer.dmabuf_formats().iter().count())
            .finish_non_exhaustive()
    }
}

impl Renderer {
    /// Bring up a renderer on one device file.
    ///
    /// The device is named rather than chosen for us, because the buffers
    /// clients are asked for have to be the ones this can import: whatever
    /// device ends up here is the one [`crate::dmabuf`] offers them.
    pub fn new(path: &Path) -> Result<Self, Error> {
        let fail = |source| Error::Device {
            path: path.to_path_buf(),
            source,
        };
        let device = EGLDevice::enumerate()
            .map_err(fail)?
            .find(|device| {
                device
                    .render_device_path()
                    .is_ok_and(|device_path| device_path == path)
            })
            .ok_or(smithay::backend::egl::Error::DisplayNotSupported)
            .map_err(fail)?;

        // SAFETY: `EGLDisplay::new` requires the native display handle to
        // outlive the display; the device came from `enumerate` and is moved
        // into the display, which keeps it alive for as long as it lives.
        #[expect(
            unsafe_code,
            reason = "EGL hands out displays only through this unsafe constructor, whose contract is that the device outlives the display"
        )]
        let display = unsafe { EGLDisplay::new(device) }.map_err(fail)?;

        // No screen and no config: this renderer draws into textures it owns.
        let context = EGLContext::new(&display).map_err(fail)?;

        // SAFETY: creating a renderer from a context requires the context not
        // to be current anywhere else, and takes ownership of it. It was made
        // here a line ago and nothing else has ever seen it.
        #[expect(
            unsafe_code,
            reason = "the GLES renderer constructor is unsafe because the context must not be current elsewhere; this context was just created here and is owned by the renderer"
        )]
        let renderer = unsafe { GlesRenderer::new(context) }.map_err(|source| Error::Create {
            path: path.to_path_buf(),
            source,
        })?;

        tracing::info!(
            path = %path.display(),
            formats = renderer.dmabuf_formats().iter().count(),
            "renderer ready for GPU buffers"
        );
        Ok(Self {
            renderer,
            staging: None,
        })
    }

    /// The layouts this device can take a client's buffer in.
    ///
    /// Whatever is advertised to clients has to come from here, or they will
    /// allocate something the compositor cannot take. Only the two color
    /// layouts the compositor blends are offered, but every modifier the device
    /// reports for them is: a tiled buffer is no harder to read through the GPU
    /// than a linear one, and this is the read path that does not care.
    pub fn formats(&self) -> Vec<Format> {
        self.renderer
            .dmabuf_formats()
            .iter()
            .filter(|format| {
                matches!(format.code, Fourcc::Argb8888 | Fourcc::Xrgb8888)
                    // A modifier the client did not state says nothing about
                    // how the pixels are laid out, so it cannot be read.
                    && format.modifier != Modifier::Invalid
            })
            .copied()
            .collect()
    }

    /// Whether this renderer can bring a buffer back.
    ///
    /// Asked before a client is told that its buffer is good, and answered by
    /// doing the work on a corner of it: importing a buffer is not proof it can
    /// be read, since a driver is free to accept a layout it cannot copy out
    /// of.
    pub fn can_read(&mut self, dmabuf: &Dmabuf) -> Result<(), Error> {
        let size = dmabuf.size();
        let corner = Size::<i32, BufferCoords>::from((size.w.min(4), size.h.min(4)));
        self.with_pixels(dmabuf, Rectangle::from_size(corner), |_, _| ())
    }

    /// Bring a buffer back, and hand the pixels to [`crate::buffer`] as if they
    /// had been mapped.
    ///
    /// `format` is the layout the client's buffer was in; what comes back is
    /// that buffer's pixels in [`READBACK`]'s order, with the same meaning for
    /// the fourth byte. The compositor is told about both.
    pub fn read(
        &mut self,
        dmabuf: &Dmabuf,
        format: SourceFormat,
        scale: i32,
        destination: &mut crate::buffer::Snapshot,
    ) -> Result<(), Error> {
        let (width, height) = (dmabuf.width(), dmabuf.height());
        let size = Size::<i32, BufferCoords>::from((width as i32, height as i32));
        self.with_pixels(dmabuf, Rectangle::from_size(size), |pixels, stride| {
            destination.pixels.clear();
            destination.pixels.extend_from_slice(pixels);
            destination.width = width;
            destination.height = height;
            destination.stride = stride;
            destination.scale = scale;
            destination.format = readback_of(format);
        })
    }

    /// Draw a client's buffer into the staging texture and read `region` back
    /// out of it.
    ///
    /// The staging texture exists because a buffer imported from a client is
    /// not necessarily something the driver will draw *into*: it is whatever
    /// the client's driver felt like allocating. A texture this renderer made
    /// is.
    fn with_pixels<T>(
        &mut self,
        dmabuf: &Dmabuf,
        region: Rectangle<i32, BufferCoords>,
        consume: impl FnOnce(&[u8], u32) -> T,
    ) -> Result<T, Error> {
        let texture = self
            .renderer
            .import_dmabuf(dmabuf, None)
            .map_err(Error::Import)?;
        let size = texture.size();
        let area =
            Rectangle::<i32, Physical>::from_size(Size::<i32, Physical>::from((size.w, size.h)));

        let mut staging = match self.staging.take().filter(|staging| staging.size() == size) {
            Some(staging) => staging,
            None => self
                .renderer
                .create_buffer(READBACK, size)
                .map_err(Error::Draw)?,
        };

        {
            let mut target = self.renderer.bind(&mut staging).map_err(Error::Draw)?;
            let mut frame = self
                .renderer
                .render(&mut target, area.size, Transform::Normal)
                .map_err(Error::Draw)?;
            frame
                .clear(Color32F::TRANSPARENT, &[area])
                .map_err(Error::Draw)?;
            // Called through the trait: the renderer's own version takes a
            // shader and uniforms for the case where the caller wants to do
            // the sampling by hand.
            Frame::render_texture_from_to(
                &mut frame,
                &texture,
                Rectangle::from_size(size).to_f64(),
                area,
                // All of it: the damage is what the draw is limited to, and an
                // empty list draws nothing at all.
                &[area],
                &[],
                // The buffer's rows run top to bottom in its own memory, and
                // so do the rows a readback returns: the corners are the same
                // way up on both sides.
                Transform::Normal,
                1.0,
            )
            .map_err(Error::Draw)?;
            // End the pass explicitly, and wait for it: what is read back next
            // is only defined once the frame that drew it has been drawn, and
            // the target has to stop being held before the staging texture can
            // be bound again.
            Frame::finish(frame)
                .map_err(Error::Draw)?
                .wait()
                .map_err(Error::Sync)?;
        }

        let mapping = self
            .renderer
            .copy_texture(&staging, region, READBACK)
            .map_err(Error::Read)?;
        let pixels = self.renderer.map_texture(&mapping).map_err(Error::Read)?;
        let expected = region.size.w as usize * region.size.h as usize * 4;
        if pixels.len() < expected {
            return Err(Error::Short {
                expected,
                actual: pixels.len(),
            });
        }
        let consumed = consume(pixels, region.size.w as u32 * 4);

        self.staging = Some(staging);
        Ok(consumed)
    }
}
