//! In-process Wayland client driving a real [`Compositor`] for lifecycle
//! tests.

use std::{
    fs::File,
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    time::Duration,
};

use calloop::EventLoop;
use smithay::reexports::wayland_server::Resource;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{ObjectId, WaylandError},
    delegate_noop, event_created_child,
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::WlDataOffer,
        wl_data_source::{self, WlDataSource},
        wl_keyboard::{self, WlKeyboard},
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::XdgToplevel,
        xdg_wm_base::{self, XdgWmBase},
    },
};

use super::{Compositor, CompositorHandle, Runtime, Settings, WindowId};

/// Client-side record of the events the tests observe.
#[derive(Default)]
pub(crate) struct App {
    synced: bool,
    globals: Vec<(u32, String, u32)>,
    /// Serial of every `xdg_surface.configure`, in arrival order.
    pub configures: Vec<u32>,
    /// Surface holding keyboard focus.
    pub keyboard_focus: Option<ObjectId>,
    /// Clipboard offer the compositor most recently announced.
    pub selection: Option<WlDataOffer>,
    /// Clipboard reads the compositor requested from this client's sources,
    /// with the requested MIME type, in arrival order.
    pub sends: Vec<(WlDataSource, String, OwnedFd)>,
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.synced = true;
        }
    }
}

impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configures.push(serial);
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => state.keyboard_focus = Some(surface.id()),
            wl_keyboard::Event::Leave { .. } => state.keyboard_focus = None,
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for App {
    event_created_child!(App, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Selection { id } = event {
            state.selection = id;
        }
    }
}

impl Dispatch<WlDataSource, ()> for App {
    fn event(
        state: &mut Self,
        source: &WlDataSource,
        event: wl_data_source::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { mime_type, fd } = event {
            state.sends.push((source.clone(), mime_type, fd));
        }
    }
}

delegate_noop!(App: WlCompositor);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ignore WlSeat);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore XdgToplevel);
delegate_noop!(App: WpViewporter);
delegate_noop!(App: WpViewport);
delegate_noop!(App: WlDataDeviceManager);
delegate_noop!(App: ignore WlDataOffer);
delegate_noop!(App: ignore ZwpLinuxDmabufV1);
delegate_noop!(App: ignore ZwpLinuxBufferParamsV1);

/// `struct udmabuf_create` from `linux/udmabuf.h`.
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_CREATE`, which turns sealed memfd pages into a DMA-BUF.
struct CreateUdmabuf(UdmabufCreate);

// SAFETY: The opcode is the kernel's write-only `UDMABUF_CREATE`, the pointer
// addresses a `repr(C)` request matching `struct udmabuf_create`, and the
// kernel returns a new descriptor on success.
unsafe impl rustix::ioctl::Ioctl for CreateUdmabuf {
    type Output = OwnedFd;

    const IS_MUTATING: bool = false;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        rustix::ioctl::opcode::write::<UdmabufCreate>(b'u', 0x42)
    }

    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        (&raw mut self.0).cast()
    }

    unsafe fn output_from_ptr(
        out: rustix::ioctl::IoctlOutput,
        _: *mut std::ffi::c_void,
    ) -> rustix::io::Result<OwnedFd> {
        // SAFETY: A successful `UDMABUF_CREATE` returns a new descriptor the
        // caller now exclusively owns.
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(out) })
    }
}

/// A DMA-BUF of `pixels` backed by host memory, or `None` where `udmabuf`
/// is unavailable.
fn udmabuf(pixels: &[u8]) -> Option<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags};
    let page = rustix::param::page_size();
    let size = pixels.len().div_ceil(page) * page;
    let memfd = rustix::fs::memfd_create(
        "meowland-dmabuf",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .ok()?;
    let mut file = File::from(memfd);
    file.write_all(pixels).ok()?;
    file.set_len(u64::try_from(size).ok()?).ok()?;
    rustix::fs::fcntl_add_seals(&file, SealFlags::SHRINK).ok()?;
    let device = rustix::fs::open(
        "/dev/udmabuf",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .ok()?;
    let request = CreateUdmabuf(UdmabufCreate {
        memfd: u32::try_from(std::os::fd::AsRawFd::as_raw_fd(&file)).ok()?,
        flags: 1,
        offset: 0,
        size: u64::try_from(size).ok()?,
    });
    // SAFETY: `device` is an open udmabuf device and `request` names a live
    // sealed memfd covering whole pages.
    unsafe { rustix::ioctl::ioctl(&device, request) }.ok()
}

/// One client toplevel.
pub(crate) struct Toplevel {
    pub surface: WlSurface,
    pub xdg: XdgSurface,
    pub role: XdgToplevel,
}

/// A compositor and one connected client, pumped on the test thread.
pub(crate) struct Fixture {
    pub state: Compositor,
    runtime: Runtime,
    event_loop: EventLoop<'static, Compositor>,
    connection: Connection,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    pub app: App,
    compositor: WlCompositor,
    shm: WlShm,
    wm_base: XdgWmBase,
    viewporter: WpViewporter,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    _keyboard: WlKeyboard,
    data_devices: WlDataDeviceManager,
    data_device: WlDataDevice,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let event_loop = EventLoop::<Compositor>::try_new().expect("test event loop");
        let handle = CompositorHandle::new(event_loop.handle(), |state| state);
        let (runtime, state) =
            Runtime::new(&Settings::default(), &handle).expect("test compositor");
        let (server, client) = UnixStream::pair().expect("socket pair");
        runtime.insert_client(server);
        let connection = Connection::from_socket(client).expect("client connection");
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut partial = PartialFixture {
            event_loop,
            state,
            runtime,
            connection,
            queue,
            qh,
            app: App::default(),
        };
        partial.roundtrip();
        let compositor: WlCompositor = partial.bind(&registry, 6);
        let shm: WlShm = partial.bind(&registry, 1);
        let wm_base: XdgWmBase = partial.bind(&registry, 6);
        let viewporter: WpViewporter = partial.bind(&registry, 1);
        let dmabuf = partial
            .app
            .globals
            .iter()
            .any(|(_, global, _)| global == ZwpLinuxDmabufV1::interface().name)
            .then(|| partial.bind(&registry, 3));
        let seat: WlSeat = partial.bind(&registry, 9);
        let keyboard = seat.get_keyboard(&partial.qh, ());
        let data_devices: WlDataDeviceManager = partial.bind(&registry, 3);
        let data_device = data_devices.get_data_device(&seat, &partial.qh, ());
        partial.roundtrip();
        let PartialFixture {
            event_loop,
            state,
            runtime,
            connection,
            queue,
            qh,
            app,
        } = partial;
        Self {
            state,
            runtime,
            event_loop,
            connection,
            queue,
            qh,
            app,
            compositor,
            shm,
            wm_base,
            viewporter,
            dmabuf,
            _keyboard: keyboard,
            data_devices,
            data_device,
        }
    }

    /// Deliver every queued request and wait for the compositor's reply.
    pub(crate) fn roundtrip(&mut self) {
        pump(
            &mut self.event_loop,
            &mut self.runtime,
            &mut self.state,
            &self.connection,
            &mut self.queue,
            &self.qh,
            &mut self.app,
        );
    }

    /// Offer a new clipboard selection with the given MIME types.
    pub(crate) fn copy(&mut self, mimes: &[&str]) -> WlDataSource {
        let source = self.data_devices.create_data_source(&self.qh, ());
        for mime in mimes {
            source.offer((*mime).to_owned());
        }
        self.data_device.set_selection(Some(&source), 0);
        self.roundtrip();
        source
    }

    /// Answer the compositor's pending read of a source with `text`.
    pub(crate) fn answer(&mut self, source: &WlDataSource, text: &[u8]) -> io::Result<()> {
        let index = self
            .app
            .sends
            .iter()
            .position(|(requested, _, _)| requested == source)
            .expect("the compositor requested the source's data");
        let (_, _, fd) = self.app.sends.remove(index);
        File::from(fd).write_all(text)
    }

    /// Read the announced clipboard offer in one MIME type until EOF.
    pub(crate) fn read_selection(&mut self, mime: &str) -> Vec<u8> {
        let offer = self.app.selection.clone().expect("a clipboard offer");
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .expect("pipe");
        offer.receive(mime.to_owned(), write.as_fd());
        drop(write);
        let mut file = File::from(read);
        let mut bytes = Vec::new();
        for _ in 0..64 {
            self.roundtrip();
            match file.read_to_end(&mut bytes) {
                Ok(_) => return bytes,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("reading clipboard offer: {error}"),
            }
        }
        panic!("clipboard offer never finished");
    }

    pub(crate) fn viewport(&self, surface: &WlSurface) -> WpViewport {
        self.viewporter.get_viewport(surface, &self.qh, ())
    }

    pub(crate) fn protocol_error(&mut self) -> wayland_client::backend::protocol::ProtocolError {
        self.connection.flush().expect("flush invalid request");
        self.runtime
            .dispatch(&mut self.state)
            .expect("dispatch invalid request");
        if let Some(guard) = self.queue.prepare_read() {
            let _ = guard.read();
        }
        self.connection
            .protocol_error()
            .expect("server sent protocol error")
    }

    /// Create a toplevel with an app ID and acknowledge its initial
    /// configure.
    pub(crate) fn toplevel(&mut self, app_id: &str) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        toplevel.set_app_id(app_id.to_owned());
        surface.commit();
        self.roundtrip();
        let toplevel = Toplevel {
            surface,
            xdg,
            role: toplevel,
        };
        self.ack_configure(&toplevel);
        toplevel
    }

    /// Acknowledge the newest configure the client received.
    pub(crate) fn ack_configure(&self, toplevel: &Toplevel) {
        let serial = *self.app.configures.last().expect("a configure arrived");
        toplevel.xdg.ack_configure(serial);
    }

    /// An opaque ARGB buffer filled with one color.
    pub(crate) fn buffer(&self, width: i32, height: i32, pixel: [u8; 4]) -> WlBuffer {
        let stride = width * 4;
        let len = usize::try_from(stride * height).expect("test buffer size is positive");
        let fd = rustix::fs::memfd_create("meowland-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("memfd");
        let mut file = File::from(fd);
        file.write_all(&pixel.repeat(len / 4))
            .expect("fill test buffer");
        let pool = self
            .shm
            .create_pool(file.as_fd(), stride * height, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        buffer
    }

    /// A linear XRGB8888 DMA-BUF filled with one color, or `None` without a
    /// GPU backend or `udmabuf`.
    pub(crate) fn dmabuf(&self, width: i32, height: i32, pixel: [u8; 4]) -> Option<WlBuffer> {
        const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
        let dmabuf = self.dmabuf.as_ref()?;
        let len = usize::try_from(width * height).ok()?;
        let fd = udmabuf(&pixel.repeat(len))?;
        let params = dmabuf.create_params(&self.qh, ());
        params.add(fd.as_fd(), 0, 0, u32::try_from(width * 4).ok()?, 0, 0);
        let buffer = params.create_immed(
            width,
            height,
            XRGB8888,
            wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags::empty(),
            &self.qh,
            (),
        );
        params.destroy();
        Some(buffer)
    }

    /// Attach a fresh full-damage buffer and commit it.
    pub(crate) fn present(&mut self, surface: &WlSurface, width: i32, height: i32, pixel: [u8; 4]) {
        let buffer = self.buffer(width, height, pixel);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        self.roundtrip();
    }

    /// Compositor identity of a client toplevel.
    pub(crate) fn window(&self, toplevel: &Toplevel) -> WindowId {
        let id = toplevel.surface.id().protocol_id();
        self.state
            .windows
            .by_id
            .iter()
            .find(|(_, window)| window.surface.wl_surface().id().protocol_id() == id)
            .map(|(&window, _)| window)
            .expect("toplevel is known to the compositor")
    }
}

/// Fixture state before the globals are bound.
struct PartialFixture {
    event_loop: EventLoop<'static, Compositor>,
    state: Compositor,
    runtime: Runtime,
    connection: Connection,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    app: App,
}

impl PartialFixture {
    fn roundtrip(&mut self) {
        pump(
            &mut self.event_loop,
            &mut self.runtime,
            &mut self.state,
            &self.connection,
            &mut self.queue,
            &self.qh,
            &mut self.app,
        );
    }

    fn bind<I: Proxy + 'static>(&self, registry: &WlRegistry, version: u32) -> I
    where
        App: Dispatch<I, ()>,
    {
        let interface = I::interface().name;
        let &(name, _, advertised) = self
            .app
            .globals
            .iter()
            .find(|(_, global, _)| global == interface)
            .unwrap_or_else(|| panic!("compositor advertises {interface}"));
        registry.bind(name, version.min(advertised), &self.qh, ())
    }
}

fn pump(
    event_loop: &mut EventLoop<'static, Compositor>,
    runtime: &mut Runtime,
    state: &mut Compositor,
    connection: &Connection,
    queue: &mut EventQueue<App>,
    qh: &QueueHandle<App>,
    app: &mut App,
) {
    app.synced = false;
    let _callback = connection.display().sync(qh, ());
    for _ in 0..64 {
        connection.flush().expect("flush client requests");
        runtime.dispatch(state).expect("compositor dispatch");
        event_loop
            .dispatch(Some(Duration::ZERO), state)
            .expect("event loop dispatch");
        if let Some(guard) = queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("reading compositor events: {error}"),
            }
        }
        queue
            .dispatch_pending(app)
            .expect("dispatch compositor events");
        if app.synced {
            return;
        }
    }
    panic!("compositor never answered wl_display.sync");
}
