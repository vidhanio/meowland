//! In-process Wayland client driving a real [`Compositor`] for lifecycle
//! tests.

use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::{fd::AsFd, unix::net::UnixStream},
};

use calloop::EventLoop;
use smithay::reexports::wayland_server::Resource;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{ObjectId, WaylandError},
    delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::XdgToplevel,
    xdg_wm_base::{self, XdgWmBase},
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

delegate_noop!(App: WlCompositor);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ignore WlSeat);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore XdgToplevel);

/// One client toplevel.
pub(crate) struct Toplevel {
    pub surface: WlSurface,
    pub xdg: XdgSurface,
    _role: XdgToplevel,
}

/// A compositor and one connected client, pumped on the test thread.
pub(crate) struct Fixture {
    pub state: Compositor,
    runtime: Runtime,
    _event_loop: EventLoop<'static, Compositor>,
    connection: Connection,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    pub app: App,
    compositor: WlCompositor,
    shm: WlShm,
    wm_base: XdgWmBase,
    _keyboard: WlKeyboard,
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
        let seat: WlSeat = partial.bind(&registry, 9);
        let keyboard = seat.get_keyboard(&partial.qh, ());
        partial.roundtrip();
        let PartialFixture {
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
            _event_loop: event_loop,
            connection,
            queue,
            qh,
            app,
            compositor,
            shm,
            wm_base,
            _keyboard: keyboard,
        }
    }

    /// Deliver every queued request and wait for the compositor's reply.
    pub(crate) fn roundtrip(&mut self) {
        pump(
            &mut self.runtime,
            &mut self.state,
            &self.connection,
            &mut self.queue,
            &self.qh,
            &mut self.app,
        );
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
            _role: toplevel,
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
