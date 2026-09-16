//! Read GPU buffers back through GLES when they cannot be mapped.

use std::path::Path;

use smithay::{
    backend::{
        allocator::{Buffer as _, Format, Fourcc, Modifier, dmabuf::Dmabuf},
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind as _, Color32F, ExportMem as _, Frame, ImportDma as _, Offscreen as _,
            Renderer as _, Texture as _,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    utils::{Buffer as BufferCoords, Physical, Rectangle, Size, Transform},
};

use crate::{Error, render::SourceFormat};

/// The layout a readback is asked for: bytes in red, green, blue, alpha order.
///
/// That order is what `GL_RGBA` means, and almost every driver can write it
/// out. The client's own layout would need `GL_BGRA`, which is an extension.
const READBACK: Fourcc = Fourcc::Abgr8888;

/// The channels are in the order that [`READBACK`] writes them. The fourth byte
/// is the client's: a layout with no alpha stays without alpha, whichever way
/// round its channels are.
const fn readback_of(format: SourceFormat) -> SourceFormat {
    if format.opaque() {
        SourceFormat::Xbgr8888
    } else {
        SourceFormat::Abgr8888
    }
}

/// An offscreen renderer. Everything that it draws is drawn to be read back.
pub struct Renderer {
    renderer: GlesRenderer,
    /// The texture that buffers are drawn into.
    ///
    /// It is kept between reads, so that a client drawing at a steady size does
    /// not make the renderer allocate a texture per frame.
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
    /// The device is named rather than chosen. The buffers that clients are
    /// asked for have to be buffers that this renderer can import. The device
    /// that ends up here is therefore the one that [`crate::dmabuf`] offers to
    /// clients.
    pub fn new(path: &Path) -> Result<Self, Error> {
        let device = EGLDevice::enumerate()?
            .find(|device| {
                device
                    .render_device_path()
                    .is_ok_and(|device_path| device_path == path)
            })
            .ok_or(smithay::backend::egl::Error::DisplayNotSupported)?;

        // SAFETY: `EGLDisplay::new` requires the native display handle to
        // outlive the display. The device comes from `enumerate`, is moved into
        // the display, and the display holds it.
        #[expect(
            unsafe_code,
            reason = "EGL hands out displays only through this unsafe constructor, whose contract is that the device outlives the display"
        )]
        let display = unsafe { EGLDisplay::new(device) }?;

        // No screen and no config: this renderer draws into textures that it
        // owns.
        let context = EGLContext::new(&display)?;

        // SAFETY: creating a renderer from a context requires the context not
        // to be current anywhere else, and takes ownership of it. It was made
        // here a line ago and nothing else has ever seen it.
        #[expect(
            unsafe_code,
            reason = "the GLES renderer constructor is unsafe because the context must not be current elsewhere; this context was just created here and is owned by the renderer"
        )]
        let renderer = unsafe { GlesRenderer::new(context) }?;

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

    /// The layouts in which this device can take a client's buffer.
    ///
    /// Everything that is advertised to clients has to come from here, or a
    /// client allocates something that the compositor cannot take. Only the two
    /// color layouts that the compositor blends are offered, but every modifier
    /// that the device reports for them is offered too. A tiled buffer is no
    /// harder to read through the GPU than a linear one, and this read path is
    /// the same for every modifier.
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
    /// The caller asks before the compositor reports the buffer to the client
    /// as good. The answer comes from doing the work on a corner of the
    /// buffer. An import does not prove that a buffer can be read: a driver
    /// may accept a layout that it cannot copy out of.
    pub fn can_read(&mut self, dmabuf: &Dmabuf) -> Result<(), Error> {
        let size = dmabuf.size();
        let corner = Size::<i32, BufferCoords>::from((size.w.min(4), size.h.min(4)));
        self.with_pixels(dmabuf, Rectangle::from_size(corner), |_, _| ())
    }

    /// Bring a buffer back, and hand the pixels to [`crate::buffer`].
    ///
    /// `format` is the layout that the client's buffer was in. What comes back
    /// is that buffer's pixels in the order of [`READBACK`], with the same
    /// meaning for the fourth byte. Both are recorded, and the pixels look like
    /// the pixels of a mapped buffer.
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
            destination.fill(pixels, stride, width, height, scale, readback_of(format));
        })
    }

    /// Draw a client's buffer into the staging texture, and read `region` back.
    ///
    /// The staging texture exists because a buffer imported from a client is
    /// not necessarily a buffer that the driver will draw into. It is
    /// whatever the client's driver allocated. A texture that this renderer
    /// made is.
    fn with_pixels<T>(
        &mut self,
        dmabuf: &Dmabuf,
        region: Rectangle<i32, BufferCoords>,
        consume: impl FnOnce(&[u8], u32) -> T,
    ) -> Result<T, Error> {
        let texture = self
            .renderer
            .import_dmabuf(dmabuf, None)
            .map_err(Error::Gles)?;
        let size = texture.size();
        let area =
            Rectangle::<i32, Physical>::from_size(Size::<i32, Physical>::from((size.w, size.h)));

        let mut staging = match self.staging.take().filter(|staging| staging.size() == size) {
            Some(staging) => staging,
            None => self
                .renderer
                .create_buffer(READBACK, size)
                .map_err(Error::Gles)?,
        };

        {
            let mut target = self.renderer.bind(&mut staging).map_err(Error::Gles)?;
            let mut frame = self
                .renderer
                .render(&mut target, area.size, Transform::Normal)
                .map_err(Error::Gles)?;
            frame
                .clear(Color32F::TRANSPARENT, &[area])
                .map_err(Error::Gles)?;
            // Called through the trait: the renderer's own version takes a
            // shader and uniforms, for a caller that does the
            // sampling by hand.
            Frame::render_texture_from_to(
                &mut frame,
                &texture,
                Rectangle::from_size(size).to_f64(),
                area,
                // All of it: the damage list is what the draw is limited to, and
                // an empty list draws nothing.
                &[area],
                &[],
                // The buffer's rows run top to bottom in its own memory, and so
                // do the rows that a readback returns. The corners are the same
                // way up on both sides.
                Transform::Normal,
                1.0,
            )
            .map_err(Error::Gles)?;
            // End the pass explicitly, and wait for it. The next readback is
            // defined only after the frame that drew it, and the target has to
            // stop being held before the staging texture can be bound again.
            Frame::finish(frame)
                .map_err(Error::Gles)?
                .wait()
                .map_err(Error::Sync)?;
        }

        let mapping = self
            .renderer
            .copy_texture(&staging, region, READBACK)
            .map_err(Error::Gles)?;
        let pixels = self.renderer.map_texture(&mapping).map_err(Error::Gles)?;
        let expected = region.size.w as usize * region.size.h as usize * 4;
        if pixels.len() < expected {
            return Err(Error::ShortReadback {
                expected,
                actual: pixels.len(),
            });
        }
        let consumed = consume(pixels, region.size.w as u32 * 4);

        self.staging = Some(staging);
        Ok(consumed)
    }
}
