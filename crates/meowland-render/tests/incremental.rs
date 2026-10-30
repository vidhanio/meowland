//! Damage-limited recomposition driven by real `wl_surface` commits.
//!
//! An in-process Wayland client commits shm buffers to a Smithay display whose
//! commit handler feeds snapshots and surface damage to a [`Scene`]. After
//! every frame, the retained pane must equal a from-scratch composition of the
//! same surfaces, and a pane that only applies the reported rectangle must
//! too.

use std::{
    collections::HashMap,
    fs::File,
    io::{ErrorKind, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::Arc,
};

use meowland_core::pixels::FrameSize;
use meowland_render::{Changed, Renderer, Scene, Snapshot, SurfaceDamage, surface_geometry};
use rustix::fs::{MemfdFlags, memfd_create};
use smithay::{
    delegate_dispatch2,
    reexports::wayland_server::{
        self, Display,
        backend::ClientData,
        protocol::{wl_buffer, wl_surface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, with_states,
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        viewporter::ViewporterState,
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    backend::WaylandError,
    delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_output::Transform,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

/// Compositor half: records surface damage as the production commit handler
/// does, but copies every buffer whole so client damage is the only signal the
/// renderer gets about what changed.
struct Server {
    compositor: CompositorState,
    shm: ShmState,
    _viewporter: ViewporterState,
    root: Option<wl_surface::WlSurface>,
    snapshots: HashMap<wl_surface::WlSurface, Snapshot>,
    scene: Scene,
}

#[derive(Default)]
struct Peer {
    compositor: CompositorClientState,
}

impl ClientData for Peer {}

impl BufferHandler for Server {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Server {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<Peer>().expect("peer data").compositor
    }

    fn new_surface(&mut self, surface: &wl_surface::WlSurface) {
        self.root.get_or_insert_with(|| surface.clone());
    }

    fn commit(&mut self, surface: &wl_surface::WlSurface) {
        let (assignment, damage) = with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceAttributes>();
            let current = cached.current();
            let committed = (current.buffer.take(), std::mem::take(&mut current.damage));
            drop(cached);
            committed
        });
        let new_buffer = matches!(assignment, Some(BufferAssignment::NewBuffer(_)));
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let mut previous = self.snapshots.remove(surface);
                let snapshot = with_buffer_contents(&buffer, |ptr, len, data| {
                    // SAFETY: Smithay keeps the mapped pool readable for this
                    // callback and supplies its pointer and length together.
                    unsafe { Snapshot::copy_shm(ptr, len, &data, None, &[], &mut previous) }
                })
                .expect("test buffers are shm")
                .expect("test buffers are valid ARGB8888");
                buffer.release();
                self.snapshots.insert(surface.clone(), snapshot);
            }
            Some(BufferAssignment::Removed) => {
                self.snapshots.remove(surface);
            }
            None => {}
        }
        if let Some(snapshot) = self.snapshots.get(surface) {
            let geometry = surface_geometry(surface, snapshot);
            let damage = if new_buffer && damage.is_empty() {
                SurfaceDamage::full(&geometry)
            } else {
                SurfaceDamage::new(&damage, &geometry)
            };
            self.scene.damage(surface, damage);
        }
    }
}

delegate_dispatch2!(Server);

/// Client half: collects globals and observes `wl_display.sync` completion.
#[derive(Default)]
struct App {
    globals: Vec<(u32, String)>,
    synced: bool,
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
            name, interface, ..
        } = event
        {
            state.globals.push((name, interface));
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

delegate_noop!(App: WlCompositor);
delegate_noop!(App: WlSubcompositor);
delegate_noop!(App: WlSubsurface);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: WpViewporter);
delegate_noop!(App: WpViewport);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WlBuffer);

/// Both ends of one in-process Wayland connection, pumped on this thread.
struct Wire {
    display: Display<Server>,
    server: Server,
    connection: Connection,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    app: App,
}

impl Wire {
    fn new() -> Self {
        let display = Display::<Server>::new().expect("server display");
        let handle = display.handle();
        let server = Server {
            compositor: CompositorState::new::<Server>(&handle),
            shm: ShmState::new::<Server>(&handle, []),
            _viewporter: ViewporterState::new::<Server>(&handle),
            root: None,
            snapshots: HashMap::new(),
            scene: Scene::default(),
        };
        let (server_socket, client_socket) = UnixStream::pair().expect("socket pair");
        display
            .handle()
            .insert_client(server_socket, Arc::new(Peer::default()))
            .expect("register client");
        let connection = Connection::from_socket(client_socket).expect("client connection");
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        Self {
            display,
            server,
            connection,
            queue,
            qh,
            app: App::default(),
        }
    }

    /// Deliver every queued request to the server and wait for its reply.
    fn roundtrip(&mut self) {
        self.app.synced = false;
        let _callback = self.connection.display().sync(&self.qh, ());
        for _ in 0..64 {
            self.connection.flush().expect("client flush");
            self.display
                .dispatch_clients(&mut self.server)
                .expect("server dispatch");
            self.display.flush_clients().expect("server flush");
            if let Some(guard) = self.queue.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => panic!("client read failed: {error}"),
                }
            }
            self.queue
                .dispatch_pending(&mut self.app)
                .expect("client dispatch");
            if self.app.synced {
                return;
            }
        }
        panic!("server never answered wl_display.sync");
    }
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

const fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Premultiplied RGBA pixels the client owns and re-uploads on every attach.
struct Image {
    width: i32,
    height: i32,
    pixels: Vec<[u8; 4]>,
}

impl Image {
    fn new(width: i32, height: i32, paint: impl Fn(i32, i32) -> [u8; 4]) -> Self {
        let mut image = Self {
            width,
            height,
            pixels: vec![[0; 4]; usize::try_from(width * height).expect("positive image size")],
        };
        image.paint(rect(0, 0, width, height), paint);
        image
    }

    fn paint(&mut self, area: Rect, paint: impl Fn(i32, i32) -> [u8; 4]) {
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let index = usize::try_from(y * self.width + x).expect("pixel inside the image");
                self.pixels[index] = paint(x, y);
            }
        }
    }
}

fn channel(seed: u8, x: i32, y: i32, salt: u8) -> u8 {
    let x = u8::try_from(x).expect("pattern column fits a byte");
    let y = u8::try_from(y).expect("pattern row fits a byte");
    seed.wrapping_mul(29)
        .wrapping_add(x.wrapping_mul(37))
        .wrapping_add(y.wrapping_mul(53))
        .wrapping_add(salt.wrapping_mul(71))
}

fn opaque(seed: u8) -> impl Fn(i32, i32) -> [u8; 4] {
    move |x, y| {
        [
            channel(seed, x, y, 0),
            channel(seed, x, y, 1),
            channel(seed, x, y, 2),
            255,
        ]
    }
}

/// Mixes fully transparent, partially transparent, and opaque pixels.
fn translucent(seed: u8) -> impl Fn(i32, i32) -> [u8; 4] {
    move |x, y| {
        let alpha = match channel(seed, x, y, 3) % 4 {
            0 => 0,
            1 => 96,
            2 => 180,
            _ => 255,
        };
        let premultiply = |value: u8| {
            u8::try_from(u16::from(value) * u16::from(alpha) / 255).expect("product fits a byte")
        };
        [
            premultiply(channel(seed, x, y, 0)),
            premultiply(channel(seed, x, y, 1)),
            premultiply(channel(seed, x, y, 2)),
            alpha,
        ]
    }
}

struct Surface {
    wl: WlSurface,
    image: Image,
    subsurface: Option<WlSubsurface>,
}

impl Surface {
    const fn subsurface(&self) -> &WlSubsurface {
        self.subsurface.as_ref().expect("surface is a subsurface")
    }
}

/// How a repaint tells the compositor what changed.
#[derive(Clone, Copy)]
enum Hint {
    /// `wl_surface.damage_buffer` over the painted rectangle.
    Buffer,
    /// `wl_surface.damage` over the painted rectangle, valid only for
    /// surfaces whose buffer maps one-to-one onto surface coordinates.
    Surface,
    /// No damage request, which marks the whole new buffer as changed.
    Absent,
}

/// What a frame is allowed to report, beyond matching full composition.
#[derive(Clone, Copy, Debug)]
enum Extent {
    /// Geometry changed; any rectangle is acceptable.
    Structural,
    /// Content damage must be recomposed without resending the whole pane.
    Partial,
    /// The damaged pixels are invisible, so no frame may be produced.
    Unchanged,
}

struct Fixture {
    wire: Wire,
    compositor: WlCompositor,
    subcompositor: WlSubcompositor,
    shm: WlShm,
    viewporter: WpViewporter,
    attached: Vec<(WlBuffer, File)>,
    renderer: Renderer,
    presented: Vec<u8>,
    size: FrameSize,
    scale: f64,
}

impl Fixture {
    fn new(scale: f64, (width, height): (u32, u32)) -> Self {
        let mut wire = Wire::new();
        let registry = wire.connection.display().get_registry(&wire.qh, ());
        wire.roundtrip();
        let global = |interface: &str| {
            wire.app
                .globals
                .iter()
                .find_map(|(name, advertised)| (advertised == interface).then_some(*name))
                .unwrap_or_else(|| panic!("{interface} is not advertised"))
        };
        let compositor = registry.bind(global("wl_compositor"), 5, &wire.qh, ());
        let subcompositor = registry.bind(global("wl_subcompositor"), 1, &wire.qh, ());
        let shm = registry.bind(global("wl_shm"), 1, &wire.qh, ());
        let viewporter = registry.bind(global("wp_viewporter"), 1, &wire.qh, ());
        let size = FrameSize::new(width, height).expect("valid pane size");
        Self {
            wire,
            compositor,
            subcompositor,
            shm,
            viewporter,
            attached: Vec::new(),
            renderer: Renderer::default(),
            presented: vec![0; size.rgb_len()],
            size,
            scale,
        }
    }

    fn surface(&self, image: Image) -> Surface {
        Surface {
            wl: self.compositor.create_surface(&self.wire.qh, ()),
            image,
            subsurface: None,
        }
    }

    fn subsurface(&self, parent: &Surface, image: Image, (x, y): (i32, i32)) -> Surface {
        let mut surface = self.surface(image);
        let subsurface =
            self.subcompositor
                .get_subsurface(&surface.wl, &parent.wl, &self.wire.qh, ());
        subsurface.set_position(x, y);
        surface.subsurface = Some(subsurface);
        surface
    }

    fn viewport(&self, surface: &Surface) -> WpViewport {
        self.viewporter.get_viewport(&surface.wl, &self.wire.qh, ())
    }

    /// Upload the surface's image into a fresh shm buffer and attach it.
    fn attach(&mut self, surface: &Surface) {
        let image = &surface.image;
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|&[red, green, blue, alpha]| [blue, green, red, alpha])
            .collect();
        let mut file =
            File::from(memfd_create("meowland-render-test", MemfdFlags::CLOEXEC).expect("memfd"));
        file.write_all(&bytes).expect("fill shm buffer");
        let pool = self.shm.create_pool(
            file.as_fd(),
            i32::try_from(bytes.len()).expect("pool size fits i32"),
            &self.wire.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            image.width,
            image.height,
            image.width * 4,
            wl_shm::Format::Argb8888,
            &self.wire.qh,
            (),
        );
        pool.destroy();
        surface.wl.attach(Some(&buffer), 0, 0);
        self.attached.push((buffer, file));
    }

    /// Repaint part of a surface, attach the result, and commit it.
    fn repaint(
        &mut self,
        surface: &mut Surface,
        area: Rect,
        paint: impl Fn(i32, i32) -> [u8; 4],
        hint: Hint,
    ) {
        surface.image.paint(area, paint);
        self.attach(surface);
        match hint {
            Hint::Buffer => surface
                .wl
                .damage_buffer(area.x, area.y, area.width, area.height),
            Hint::Surface => surface.wl.damage(area.x, area.y, area.width, area.height),
            Hint::Absent => {}
        }
        surface.wl.commit();
    }

    /// Render the committed scene incrementally and compare it with a full
    /// recomposition.
    fn frame(&mut self, step: &str, extent: Extent) {
        self.wire.roundtrip();
        for (buffer, _) in self.attached.drain(..) {
            buffer.destroy();
        }
        let server = &mut self.wire.server;
        let root = server.root.as_ref();
        let changed = self.renderer.update(
            &mut server.scene,
            root,
            &server.snapshots,
            self.size,
            self.scale,
            |_, _| {},
        );
        let mut full = Scene::default();
        Renderer::default().update(
            &mut full,
            root,
            &server.snapshots,
            self.size,
            self.scale,
            |_, _| {},
        );
        let width = self.size.width();
        assert_same_frame(
            server.scene.pixels(),
            full.pixels(),
            width,
            &format!("{step}: retained pane diverged from full recomposition"),
        );
        if let Some(changed) = &changed {
            apply(
                &mut self.presented,
                server.scene.pixels(),
                changed,
                self.size,
            );
        }
        assert_same_frame(
            &self.presented,
            full.pixels(),
            width,
            &format!("{step}: reported rectangle {changed:?} missed a changed pixel"),
        );
        let pane_area = u64::from(width) * u64::from(self.size.height());
        match extent {
            Extent::Structural => {}
            Extent::Partial => {
                let changed = changed.unwrap_or_else(|| {
                    panic!("{step}: no visible pixel changed, so the step exercises nothing")
                });
                let area = u64::from(changed.columns.end - changed.columns.start)
                    * u64::from(changed.rows.end - changed.rows.start);
                assert!(
                    area < pane_area,
                    "{step}: partial damage resent the whole pane as {changed:?}"
                );
            }
            Extent::Unchanged => assert!(
                changed.is_none(),
                "{step}: invisible damage produced a frame {changed:?}"
            ),
        }
    }
}

/// Copy the reported rectangle into a pane that only ever receives bands.
fn apply(presented: &mut [u8], composed: &[u8], changed: &Changed, size: FrameSize) {
    let columns = usize::try_from(changed.columns.start).expect("column fits usize") * 3
        ..usize::try_from(changed.columns.end).expect("column fits usize") * 3;
    for row in changed.rows.clone() {
        let start = usize::try_from(row).expect("row fits usize") * size.rgb_stride();
        let span = start + columns.start..start + columns.end;
        presented[span.clone()].copy_from_slice(&composed[span]);
    }
}

fn assert_same_frame(got: &[u8], want: &[u8], width: u32, context: &str) {
    assert_eq!(got.len(), want.len(), "{context}: frame sizes differ");
    let width = usize::try_from(width).expect("pane width fits usize");
    if let Some(index) = got
        .as_chunks::<3>()
        .0
        .iter()
        .zip(want.as_chunks::<3>().0)
        .position(|(got, want)| got != want)
    {
        panic!(
            "{context}: first mismatch at ({}, {}): got {:?}, want {:?}",
            index % width,
            index / width,
            &got[index * 3..index * 3 + 3],
            &want[index * 3..index * 3 + 3],
        );
    }
}

/// Drive overlapping, clipped, scaled, rotated, and cropped surfaces through
/// partial commits, checking every frame against full composition.
fn damage_limited_frames_match_full_composition(scale: f64, size: (u32, u32)) {
    let mut fixture = Fixture::new(scale, size);
    let mut root = fixture.surface(Image::new(20, 14, translucent(1)));
    let mut edge = fixture.subsurface(&root, Image::new(12, 8, opaque(2)), (15, 10));
    let mut rotated = fixture.subsurface(&root, Image::new(12, 8, translucent(3)), (-2, 3));
    rotated.wl.set_buffer_scale(2);
    rotated.wl.set_buffer_transform(Transform::_90);
    let mut cropped = fixture.subsurface(&root, Image::new(10, 10, translucent(4)), (8, 5));
    let viewport = fixture.viewport(&cropped);
    viewport.set_source(2.0, 2.0, 6.0, 4.0);
    viewport.set_destination(12, 8);
    for surface in [&edge, &rotated, &cropped, &root] {
        fixture.attach(surface);
        surface.wl.commit();
    }
    fixture.frame("initial commit", Extent::Structural);

    fixture.repaint(&mut root, rect(6, 6, 6, 4), translucent(5), Hint::Surface);
    fixture.frame("root damage under translucent overlays", Extent::Partial);

    fixture.repaint(&mut edge, rect(6, 3, 6, 5), opaque(6), Hint::Buffer);
    root.wl.commit();
    fixture.frame("damage clipped by the pane corner", Extent::Partial);

    fixture.repaint(
        &mut rotated,
        rect(1, 3, 11, 5),
        translucent(8),
        Hint::Buffer,
    );
    root.wl.commit();
    fixture.frame(
        "odd buffer damage on a scaled rotated surface",
        Extent::Partial,
    );

    fixture.repaint(&mut cropped, rect(0, 0, 5, 5), translucent(8), Hint::Buffer);
    root.wl.commit();
    fixture.frame("damage straddling the viewport source", Extent::Partial);

    fixture.repaint(&mut cropped, rect(8, 7, 2, 3), opaque(9), Hint::Buffer);
    root.wl.commit();
    fixture.frame("damage outside the viewport source", Extent::Unchanged);

    fixture.repaint(
        &mut cropped,
        rect(6, 4, 2, 2),
        translucent(10),
        Hint::Buffer,
    );
    fixture.repaint(&mut root, rect(0, 0, 3, 2), translucent(11), Hint::Surface);
    fixture.frame("damage on two surfaces at once", Extent::Partial);

    fixture.repaint(&mut root, rect(16, 11, 3, 2), opaque(12), Hint::Surface);
    fixture.frame(
        "root damage occluded by an opaque subsurface",
        Extent::Unchanged,
    );

    edge.subsurface().set_desync();
    fixture.repaint(&mut edge, rect(0, 0, 3, 3), opaque(13), Hint::Buffer);
    fixture.frame("desynchronized subsurface commit", Extent::Partial);

    fixture.repaint(
        &mut cropped,
        rect(0, 0, 10, 10),
        translucent(14),
        Hint::Absent,
    );
    root.wl.commit();
    fixture.frame("new buffer without damage hints", Extent::Partial);

    rotated.subsurface().set_position(1, 2);
    root.wl.commit();
    fixture.frame("subsurface moved", Extent::Structural);

    cropped.subsurface().set_position(-1, -1);
    viewport.set_destination(26, 18);
    fixture.repaint(&mut cropped, rect(0, 0, 10, 10), opaque(15), Hint::Buffer);
    root.wl.commit();
    fixture.frame("opaque surface covers the pane", Extent::Structural);

    fixture.repaint(&mut cropped, rect(3, 3, 2, 2), opaque(16), Hint::Buffer);
    root.wl.commit();
    fixture.frame("damage on the covering surface", Extent::Partial);

    fixture.repaint(&mut root, rect(11, 3, 6, 6), translucent(17), Hint::Surface);
    fixture.frame(
        "damage hidden under the covering surface",
        Extent::Unchanged,
    );

    fixture.repaint(
        &mut cropped,
        rect(5, 3, 1, 1),
        |_, _| [20, 30, 40, 128],
        Hint::Buffer,
    );
    root.wl.commit();
    fixture.frame("translucent pixel reveals hidden damage", Extent::Partial);

    cropped.wl.attach(None, 0, 0);
    cropped.wl.commit();
    root.wl.commit();
    fixture.frame("covering buffer removed", Extent::Structural);
}

#[test]
fn damage_limited_frames_match_full_composition_at_unit_scale() {
    damage_limited_frames_match_full_composition(1.0, (24, 16));
}

#[test]
fn damage_limited_frames_match_full_composition_at_fractional_scale() {
    damage_limited_frames_match_full_composition(1.5, (36, 24));
}

#[test]
fn damage_limited_frames_match_full_composition_when_downscaled() {
    damage_limited_frames_match_full_composition(0.5, (12, 8));
}
