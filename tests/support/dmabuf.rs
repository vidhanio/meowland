use std::{error::Error, fs::OpenOptions, path::PathBuf};

use smithay::{
    backend::{
        allocator::{
            Allocator, Buffer, Format, Fourcc, Modifier,
            dmabuf::{AsDmabuf, Dmabuf},
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
        },
        drm::NodeType,
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind, Color32F, Frame, Renderer,
            gles::{Capability, GlesRenderer},
            sync::SyncPoint,
        },
    },
    utils::{Rectangle, Transform},
};

use super::{Client, u32s};

type TestError = Box<dyn Error>;

pub struct Producer {
    pub node: PathBuf,
    allocator: GbmAllocator<std::fs::File>,
    renderer: GlesRenderer,
    formats: Vec<Format>,
}

pub struct GpuBuffer {
    _allocation: GbmBuffer,
    pub dmabuf: Dmabuf,
}

impl Producer {
    pub fn discover() -> Option<Self> {
        let selection = std::env::var_os("MEOWLAND_RENDER_NODE");
        if selection.as_deref() == Some(std::ffi::OsStr::new("off")) {
            eprintln!("SKIP DMA-BUF GPU scenario: MEOWLAND_RENDER_NODE=off");
            return None;
        }
        let devices = match EGLDevice::enumerate() {
            Ok(devices) => devices,
            Err(error) => {
                eprintln!("SKIP DMA-BUF GPU scenario: EGL device enumeration failed: {error}");
                return None;
            }
        };
        let mut failures = Vec::new();
        for device in devices {
            let Ok(Some(drm_node)) = device.try_get_render_node() else {
                continue;
            };
            if drm_node.ty() != NodeType::Render {
                continue;
            }
            let Some(node) = drm_node.dev_path() else {
                continue;
            };
            if let Some(selected) = selection.as_deref()
                && selected != std::ffi::OsStr::new("auto")
                && selected != node.as_os_str()
            {
                continue;
            }
            match Self::initialize(device, node.clone()) {
                Ok(mut producer) => {
                    let formats = producer.formats.clone();
                    match producer.allocate(4, 3, &formats).and_then(|mut buffer| {
                        producer.paint(&mut buffer, &[[0, 0, 0, 255]; 12])?.wait()?;
                        Ok(())
                    }) {
                        Ok(()) => return Some(producer),
                        Err(error) => failures.push(format!("{}: {error}", node.display())),
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", node.display())),
            }
        }
        eprintln!(
            "SKIP DMA-BUF GPU scenario: no usable EGL/GBM render node; {}",
            if failures.is_empty() {
                "no matching EGL device with a DRM render node".to_owned()
            } else {
                failures.join("; ")
            }
        );
        None
    }

    #[allow(unsafe_code)]
    fn initialize(device: EGLDevice, node: PathBuf) -> Result<Self, TestError> {
        let fd = OpenOptions::new().read(true).write(true).open(&node)?;
        let allocator = GbmAllocator::new(GbmDevice::new(fd)?, GbmBufferFlags::RENDERING);
        // SAFETY: this owned EGL device/display/context is used only on the
        // test thread.
        let display = unsafe { EGLDisplay::new(device)? };
        if !display
            .extensions()
            .iter()
            .any(|extension| extension == "EGL_KHR_fence_sync")
        {
            return Err("GPU scenario requires EGL fence synchronization".into());
        }
        let formats = display.dmabuf_render_formats().iter().copied().collect();
        let context = EGLContext::new(&display)?;
        // SAFETY: the freshly created context is not current on another thread.
        let renderer = unsafe { GlesRenderer::new(context)? };
        if !renderer.capabilities().contains(&Capability::Fencing)
            || !renderer.capabilities().contains(&Capability::ExportFence)
        {
            return Err("GPU scenario requires GLES 3 and EGL synchronization".into());
        }
        Ok(Self {
            node,
            allocator,
            renderer,
            formats,
        })
    }

    pub fn allocate(
        &mut self,
        width: u32,
        height: u32,
        advertised: &[Format],
    ) -> Result<GpuBuffer, TestError> {
        let mut failures = Vec::new();
        for format in advertised.iter().filter(|format| {
            matches!(format.code, Fourcc::Argb8888 | Fourcc::Abgr8888)
                && self.formats.contains(format)
        }) {
            match self
                .allocator
                .create_buffer(width, height, format.code, &[format.modifier])
            {
                Ok(allocation) => {
                    // GBM may fall back to implicit modifiers: verify the
                    // actual export too.
                    if !advertised.contains(&allocation.format())
                        || !self.formats.contains(&allocation.format())
                    {
                        continue;
                    }
                    let mut dmabuf = match allocation.export() {
                        Ok(dmabuf) => dmabuf,
                        Err(error) => {
                            failures.push(error.to_string());
                            continue;
                        }
                    };
                    if let Err(error) = self.renderer.bind(&mut dmabuf) {
                        failures.push(error.to_string());
                        continue;
                    }
                    return Ok(GpuBuffer {
                        _allocation: allocation,
                        dmabuf,
                    });
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        Err(format!(
            "no allocatable advertised RGBA format/modifier: {}",
            failures.join("; ")
        )
        .into())
    }

    pub fn paint(
        &mut self,
        buffer: &mut GpuBuffer,
        rgba: &[[u8; 4]],
    ) -> Result<SyncPoint, TestError> {
        let size = buffer.dmabuf.size();
        assert_eq!(rgba.len(), (size.w * size.h) as usize);
        let mut target = self.renderer.bind(&mut buffer.dmabuf)?;
        let mut frame =
            self.renderer
                .render(&mut target, (size.w, size.h).into(), Transform::Normal)?;
        for (index, pixel) in rgba.iter().enumerate() {
            let x = index as i32 % size.w;
            let y = index as i32 / size.w;
            frame.clear(
                Color32F::new(
                    f32::from(pixel[0]) / 255.0,
                    f32::from(pixel[1]) / 255.0,
                    f32::from(pixel[2]) / 255.0,
                    f32::from(pixel[3]) / 255.0,
                ),
                &[Rectangle::new((x, y).into(), (1, 1).into())],
            )?;
        }
        // Submit, but do not wait: the server must honor the implicit writer
        // fence.
        Ok(frame.finish()?)
    }
}

pub struct DmabufProtocol {
    object: u32,
    pub formats: Vec<Format>,
}

impl DmabufProtocol {
    pub fn bind(client: &mut Client) -> Self {
        assert!(
            client.has_global("zwp_linux_dmabuf_v1"),
            "usable GPU did not expose DMA-BUF"
        );
        let object = client.bind("zwp_linux_dmabuf_v1", 3);
        let mut formats = Vec::new();
        for message in client
            .sync()
            .into_iter()
            .filter(|message| message.object == object && message.opcode == 1)
        {
            let code = Fourcc::try_from(message.u32_at(0)).expect("advertised DRM format");
            let modifier =
                Modifier::from((u64::from(message.u32_at(1)) << 32) | u64::from(message.u32_at(2)));
            formats.push(Format { code, modifier });
        }
        assert!(!formats.is_empty(), "DMA-BUF v3 modifier list was empty");
        Self { object, formats }
    }

    fn params(&self, client: &mut Client, buffer: &GpuBuffer) -> u32 {
        let params = client.alloc();
        client.request(self.object, 1, &u32s(&[params]));
        let modifier = u64::from(buffer.dmabuf.format().modifier);
        for (plane, ((fd, offset), stride)) in buffer
            .dmabuf
            .handles()
            .zip(buffer.dmabuf.offsets())
            .zip(buffer.dmabuf.strides())
            .enumerate()
        {
            client.send_fd(
                params,
                1,
                &u32s(&[
                    plane as u32,
                    offset,
                    stride,
                    (modifier >> 32) as u32,
                    modifier as u32,
                ]),
                &fd,
            );
        }
        params
    }

    pub fn create(&self, client: &mut Client, buffer: &GpuBuffer, flags: u32) -> u32 {
        let params = self.params(client, buffer);
        let size = buffer.dmabuf.size();
        client.request(
            params,
            2,
            &u32s(&[
                size.w as u32,
                size.h as u32,
                buffer.dmabuf.format().code as u32,
                flags,
            ]),
        );
        let response = client.read_until(|message| {
            message.object == params || (message.object == 1 && message.opcode == 0)
        });
        assert_eq!(
            response.object, params,
            "DMA-BUF creation raised a protocol error: {response:?}"
        );
        assert_eq!(
            response.opcode, 0,
            "real advertised DMA-BUF import failed: {response:?}"
        );
        let buffer_id = response.u32_at(0);
        client.request(params, 0, &[]);
        buffer_id
    }

    pub fn reject_unsupported_format(&self, client: &mut Client, buffer: &GpuBuffer) {
        let params = self.params(client, buffer);
        let size = buffer.dmabuf.size();
        client.request(
            params,
            2,
            &u32s(&[size.w as u32, size.h as u32, u32::MAX, 0]),
        );
        let error = client.read_until(|message| message.object == 1 && message.opcode == 0);
        assert_eq!(
            error.u32_at(0),
            params,
            "error did not identify the bad import"
        );
    }
}

pub fn wait_release(client: &mut Client, buffer: u32, acquire: &SyncPoint) {
    client.read_until(|message| message.object == buffer && message.opcode == 0);
    assert!(
        acquire.is_reached(),
        "wl_buffer.release preceded completion of GPU writes"
    );
}
