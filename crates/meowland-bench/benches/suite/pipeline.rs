use std::{
    collections::VecDeque,
    env, fs,
    hint::black_box,
    os::fd::{AsFd, OwnedFd},
    path::PathBuf,
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use drm::{
    buffer::{Buffer as _, DrmFourcc},
    control::{Device as ControlDevice, dumbbuffer::DumbBuffer},
};
use memmap2::MmapMut;
use meowland_compositor::{Command, Event, Handle, PaneId};
use meowland_kitty::Presenter;
use meowland_protocol::{Input as PaneInput, Show};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalList, GlobalListContents},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

use crate::stats::{Rate, Row};

const BUFFERS: usize = 3;
const APP_ID: &str = "meowland-bench";
const CELL: (u16, u16) = (10, 20);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Shm,
    DmaBuf,
}

#[derive(Clone, Copy, Debug)]
enum Pattern {
    Noise,
    Video,
    Cell,
    Band,
}

struct Config {
    label: String,
    width: u32,
    height: u32,
    kind: Kind,
    pattern: Pattern,
    frames: usize,
    warmup: usize,
    input: bool,
}

#[derive(Clone)]
struct BufferState(Arc<AtomicBool>);

struct FrameBuffer {
    buffer: wl_buffer::WlBuffer,
    released: Arc<AtomicBool>,
    mapping: MmapMut,
    stride: usize,
    width: u32,
    height: u32,
    _dumb: Option<DumbBuffer>,
    _fd: Option<OwnedFd>,
}

impl FrameBuffer {
    fn upload(&mut self, frame: &[u8]) {
        let row = self.width as usize * 4;
        if self.stride == row {
            self.mapping[..row * self.height as usize].copy_from_slice(frame);
        } else {
            for y in 0..self.height as usize {
                let source = &frame[y * row..(y + 1) * row];
                let dest = &mut self.mapping[y * self.stride..y * self.stride + row];
                dest.copy_from_slice(source);
            }
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, value: u8) {
        for row in y..y + height {
            let start = row as usize * self.stride + x as usize * 4;
            self.mapping[start..start + width as usize * 4].fill(value);
        }
    }
}

enum Prepared {
    Full(Vec<Vec<u8>>),
    Cell { base: Vec<u8>, cell: (u32, u32) },
    Band { base: Vec<u8>, cell_height: u32 },
}

impl Prepared {
    fn new(config: &Config) -> Self {
        let len = config.width as usize * config.height as usize * 4;
        match config.pattern {
            Pattern::Noise => Self::Full(
                (0..4)
                    .map(|index| noise_frame(config.width, config.height, index as u32 + 1))
                    .collect(),
            ),
            Pattern::Video => Self::Full(
                (0..4)
                    .map(|index| video_frame(config.width, config.height, index as u32))
                    .collect(),
            ),
            Pattern::Cell => Self::Cell {
                base: flat_frame(len),
                cell: (10, 20),
            },
            Pattern::Band => Self::Band {
                base: flat_frame(len),
                cell_height: 20,
            },
        }
    }

    fn prepare(&self, index: usize, buffer: &mut FrameBuffer) -> (u32, u32, u32, u32) {
        match self {
            Self::Full(frames) => {
                buffer.upload(&frames[index % frames.len()]);
                (0, 0, buffer.width, buffer.height)
            }
            Self::Cell { base, cell } => {
                buffer.upload(base);
                let x = (index as u32 * cell.0 * 2) % (buffer.width - cell.0);
                let y = buffer.height / 2;
                buffer.fill_rect(x, y, cell.0, cell.1, 0xab);
                (x, y, cell.0, cell.1)
            }
            Self::Band { base, cell_height } => {
                buffer.upload(base);
                let y = (index as u32 * cell_height) % (buffer.height - cell_height);
                buffer.fill_rect(0, y, buffer.width, *cell_height, 0xab);
                (0, y, buffer.width, *cell_height)
            }
        }
    }
}

fn flat_frame(len: usize) -> Vec<u8> {
    let mut frame = vec![17; len];
    for pixel in frame.as_chunks_mut::<4>().0 {
        pixel[3] = 0xff;
    }
    frame
}

fn noise_frame(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9);
    let mut frame = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..width as usize * height as usize {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        frame.extend_from_slice(&[state as u8, (state >> 8) as u8, (state >> 16) as u8, 0xff]);
    }
    frame
}

fn video_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let noise = (x.wrapping_mul(17) ^ y.wrapping_mul(31) ^ index).wrapping_mul(0x9e37_79b9);
            frame.extend_from_slice(&[
                ((x * 255 / width) ^ (index * 7)) as u8,
                ((y * 255 / height) ^ (index * 11)) as u8,
                (noise >> 24) as u8,
                0xff,
            ]);
        }
    }
    frame
}

struct CommitLog {
    queue: VecDeque<(Instant, bool)>,
    latencies: Vec<Duration>,
    presented: usize,
    released: bool,
}

struct Session {
    commands: calloop::channel::Sender<Command>,
    display: String,
    thread: Option<thread::JoinHandle<()>>,
    receiver: Option<thread::JoinHandle<()>>,
    log: Arc<Mutex<CommitLog>>,
}

impl Session {
    fn new() -> Result<Self, String> {
        clear_stale_socket();
        let Handle {
            commands,
            mut events,
            display,
            thread,
        } = meowland_compositor::spawn().map_err(|error| error.to_string())?;
        let log = Arc::new(Mutex::new(CommitLog {
            queue: VecDeque::new(),
            latencies: Vec::new(),
            presented: 0,
            released: false,
        }));
        let receiver_log = log.clone();
        let receiver_commands = commands.clone();
        let receiver = thread::spawn(move || {
            let mut presenter = Presenter::new(Some(CELL), None);
            let mut terminal_output = Vec::new();
            while let Some(event) = events.blocking_recv() {
                match event {
                    Event::Frame {
                        pane,
                        width,
                        height,
                        y,
                        rgb,
                    } => {
                        presenter.present_into(width, height, y, rgb, &mut terminal_output);
                        black_box(terminal_output.len());
                        let now = Instant::now();
                        let mut log = receiver_log.lock().unwrap();
                        log.presented += 1;
                        let started = log
                            .queue
                            .drain(..)
                            .filter_map(|(started, record)| record.then_some(started))
                            .next_back();
                        if let Some(started) = started {
                            log.latencies.push(now.saturating_duration_since(started));
                        }
                        drop(log);
                        let _ = receiver_commands.send(Command::Ack { pane, drawn: true });
                    }
                    Event::Release { .. } => {
                        receiver_log.lock().unwrap().released = true;
                        break;
                    }
                    _ => {}
                }
            }
        });
        Ok(Self {
            commands,
            display,
            thread: Some(thread),
            receiver: Some(receiver),
            log,
        })
    }

    fn socket(&self) -> Result<PathBuf, String> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
        Ok(runtime.join(&self.display))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
    }
}

fn clear_stale_socket() {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        let socket = PathBuf::from(runtime).join(format!("wayland-meowland-{}", process::id()));
        let _ = fs::remove_file(socket);
    }
}

struct State {
    width: u32,
    height: u32,
    kind: Kind,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    seat: Option<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured: bool,
    buffers: Vec<FrameBuffer>,
    next_buffer: usize,
    prepared: Prepared,
    card: Option<Arc<Card>>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool,
    last_key: Option<u32>,
}

struct Card(fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

fn card_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("MEOWLAND_BENCH_CARD") {
        return Some(PathBuf::from(path));
    }
    (0..8)
        .map(|index| PathBuf::from(format!("/dev/dri/card{index}")))
        .find(|path| path.exists())
}

fn open_card() -> Result<Card, String> {
    let path = card_path().ok_or_else(|| "no DRM card found".to_owned())?;
    let file = fs::File::options()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    Ok(Card(file))
}

impl State {
    fn new(config: &Config) -> Self {
        let prepared = Prepared::new(config);
        Self {
            width: config.width,
            height: config.height,
            kind: config.kind,
            compositor: None,
            shm: None,
            wm_base: None,
            dmabuf: None,
            seat: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            configured: false,
            buffers: Vec::new(),
            next_buffer: 0,
            prepared,
            card: None,
            keyboard: None,
            keyboard_focus: false,
            last_key: None,
        }
    }

    fn bind(
        &mut self,
        globals: &GlobalList,
        qh: &QueueHandle<Self>,
        wants_input: bool,
    ) -> Result<(), String> {
        self.compositor = Some(
            globals
                .bind(qh, 4..=4, ())
                .map_err(|error| format!("wl_compositor: {error}"))?,
        );
        self.shm = Some(
            globals
                .bind(qh, 1..=1, ())
                .map_err(|error| format!("wl_shm: {error}"))?,
        );
        self.wm_base = Some(
            globals
                .bind(qh, 1..=1, ())
                .map_err(|error| format!("xdg_wm_base: {error}"))?,
        );
        if self.kind == Kind::DmaBuf {
            self.dmabuf = Some(
                globals
                    .bind(qh, 3..=3, ())
                    .map_err(|error| format!("zwp_linux_dmabuf_v1: {error}"))?,
            );
        }
        if wants_input {
            self.seat = Some(
                globals
                    .bind(qh, 1..=1, ())
                    .map_err(|error| format!("wl_seat: {error}"))?,
            );
        }
        Ok(())
    }

    fn init(&mut self, qh: &QueueHandle<Self>) {
        let (Some(compositor), Some(wm_base)) = (&self.compositor, &self.wm_base) else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_app_id(APP_ID.to_owned());
        toplevel.set_title("meowland benchmark".to_owned());
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }

    fn create_buffers(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let surface = self
            .surface
            .clone()
            .ok_or_else(|| "surface was not created".to_owned())?;
        match self.kind {
            Kind::Shm => {
                let shm = self
                    .shm
                    .as_ref()
                    .ok_or_else(|| "wl_shm was not bound".to_owned())?;
                let stride = self.width as usize * 4;
                let len = stride * self.height as usize;
                for _ in 0..BUFFERS {
                    let fd = memfd_create("meowland-bench", MemfdFlags::CLOEXEC)
                        .map_err(|error| format!("memfd_create: {error}"))?;
                    ftruncate(&fd, len as u64).map_err(|error| format!("ftruncate: {error}"))?;
                    let mapping = unsafe { MmapMut::map_mut(&fd) }
                        .map_err(|error| format!("mmap: {error}"))?;
                    let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
                    let released = Arc::new(AtomicBool::new(true));
                    let buffer = pool.create_buffer(
                        0,
                        self.width as i32,
                        self.height as i32,
                        stride as i32,
                        wl_shm::Format::Xrgb8888,
                        qh,
                        BufferState(released.clone()),
                    );
                    pool.destroy();
                    self.buffers.push(FrameBuffer {
                        buffer,
                        released,
                        mapping,
                        stride,
                        width: self.width,
                        height: self.height,
                        _dumb: None,
                        _fd: None,
                    });
                }
            }
            Kind::DmaBuf => {
                let dmabuf = self
                    .dmabuf
                    .as_ref()
                    .ok_or_else(|| "zwp_linux_dmabuf_v1 was not bound".to_owned())?;
                let card = Arc::new(open_card()?);
                for _ in 0..BUFFERS {
                    let dumb = card
                        .create_dumb_buffer((self.width, self.height), DrmFourcc::Xrgb8888, 32)
                        .map_err(|error| format!("create_dumb_buffer: {error}"))?;
                    let stride = dumb.pitch() as usize;
                    let fd = card
                        .buffer_to_prime_fd(dumb.handle(), drm::RDWR)
                        .map_err(|error| format!("buffer_to_prime_fd: {error}"))?;
                    let mapping = unsafe { MmapMut::map_mut(&fd) }
                        .map_err(|error| format!("mapping dmabuf: {error}"))?;
                    let params = dmabuf.create_params(qh, ());
                    let modifier = 0u64;
                    params.add(
                        fd.as_fd(),
                        0,
                        0,
                        stride as u32,
                        (modifier >> 32) as u32,
                        modifier as u32,
                    );
                    let released = Arc::new(AtomicBool::new(true));
                    let buffer = params.create_immed(
                        self.width as i32,
                        self.height as i32,
                        DrmFourcc::Xrgb8888 as u32,
                        zwp_linux_buffer_params_v1::Flags::empty(),
                        qh,
                        BufferState(released.clone()),
                    );
                    params.destroy();
                    self.buffers.push(FrameBuffer {
                        buffer,
                        released,
                        mapping,
                        stride,
                        width: self.width,
                        height: self.height,
                        _dumb: Some(dumb),
                        _fd: Some(fd),
                    });
                }
                self.card = Some(card);
            }
        }
        if self.configured && !self.buffers.is_empty() {
            surface.attach(Some(&self.buffers[0].buffer), 0, 0);
            surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
            surface.commit();
        }
        Ok(())
    }

    fn commit_next(
        &mut self,
        qh: &QueueHandle<Self>,
        event_queue: &mut EventQueue<Self>,
        log: &Arc<Mutex<CommitLog>>,
        index: usize,
        record: bool,
    ) -> Result<(), String> {
        let buffer_index = self.next_buffer;
        let released = self.buffers[buffer_index].released.clone();
        while !released.swap(false, Ordering::AcqRel) {
            event_queue
                .blocking_dispatch(self)
                .map_err(|error| format!("waiting for buffer release: {error}"))?;
        }
        let buffer = &mut self.buffers[buffer_index];
        let damage = self.prepared.prepare(index, buffer);
        let callback = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        log.lock().unwrap().queue.push_back((started, record));
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| "surface was not created".to_owned())?;
        surface.frame(qh, callback.clone());
        surface.attach(Some(&buffer.buffer), 0, 0);
        surface.damage_buffer(
            i32::try_from(damage.0).unwrap_or(i32::MAX),
            i32::try_from(damage.1).unwrap_or(i32::MAX),
            i32::try_from(damage.2).unwrap_or(i32::MAX),
            i32::try_from(damage.3).unwrap_or(i32::MAX),
        );
        surface.commit();
        self.next_buffer = (self.next_buffer + 1) % self.buffers.len();
        event_queue
            .flush()
            .map_err(|error| format!("flushing commit: {error}"))?;
        event_queue
            .dispatch_pending(self)
            .map_err(|error| format!("dispatching commit: {error}"))?;
        while !callback.load(Ordering::Acquire) {
            event_queue
                .blocking_dispatch(self)
                .map_err(|error| format!("waiting for frame callback: {error}"))?;
        }
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        _: zwp_linux_dmabuf_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            data.store(true, Ordering::Release);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferState> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferState,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            data.0.store(true, Ordering::Release);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
            if let (Some(surface), Some(buffer)) = (&state.surface, state.buffers.first()) {
                surface.attach(Some(&buffer.buffer), 0, 0);
                surface.damage_buffer(0, 0, state.width as i32, state.height as i32);
                surface.commit();
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event
            && let wayland_client::WEnum::Value(capabilities) = capabilities
            && capabilities.contains(wl_seat::Capability::Keyboard)
            && state.keyboard.is_none()
        {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key { key, .. } => state.last_key = Some(key),
            wl_keyboard::Event::Enter { .. } => state.keyboard_focus = true,
            wl_keyboard::Event::Leave { .. } => state.keyboard_focus = false,
            _ => {}
        }
    }
}

struct Client {
    state: State,
    event_queue: EventQueue<State>,
    qh: QueueHandle<State>,
    session: Session,
}

impl Client {
    fn connect(config: &Config) -> Result<Self, String> {
        let session = Session::new()?;
        let socket = session.socket()?;
        let stream = std::os::unix::net::UnixStream::connect(&socket)
            .map_err(|error| format!("connecting to {}: {error}", socket.display()))?;
        let connection = Connection::from_socket(stream)
            .map_err(|error| format!("opening Wayland connection: {error}"))?;
        let (globals, mut event_queue) =
            wayland_client::globals::registry_queue_init::<State>(&connection)
                .map_err(|error| format!("reading globals: {error}"))?;
        let qh = event_queue.handle();
        let mut state = State::new(config);
        state.bind(&globals, &qh, config.input)?;
        state.init(&qh);
        state.create_buffers(&qh)?;
        while !state.configured || state.buffers.is_empty() {
            event_queue
                .roundtrip(&mut state)
                .map_err(|error| format!("waiting for configure: {error}"))?;
        }
        session
            .commands
            .send(Command::Attach {
                pane: PaneId(1),
                show: Show::AppId(APP_ID.to_owned()),
                width: config.width,
                height: config.height,
            })
            .map_err(|error| format!("sending attach: {error}"))?;
        let mut client = Self {
            state,
            event_queue,
            qh,
            session,
        };
        client.wait_present(1)?;
        Ok(client)
    }

    fn wait_present(&mut self, target: usize) -> Result<(), String> {
        let deadline = Instant::now() + TIMEOUT;
        while self.session.log.lock().unwrap().presented < target {
            if Instant::now() > deadline {
                return Err("timed out waiting for a presented frame".to_owned());
            }
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("circulating while waiting for a frame: {error}"))?;
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn wait_queue_empty(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let (empty, released) = {
                let log = self.session.log.lock().unwrap();
                (log.queue.is_empty(), log.released)
            };
            if released {
                return Err("the compositor released the pane".to_owned());
            }
            if empty {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for frame acknowledgements".to_owned());
            }
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("circulating while waiting for acks: {error}"))?;
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn settle(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + TIMEOUT;
        let mut quiet = 0;
        while quiet < 3 {
            if Instant::now() > deadline {
                return Err("timed out waiting for the compositor to settle".to_owned());
            }
            let (pending, released) = {
                let log = self.session.log.lock().unwrap();
                (!log.queue.is_empty(), log.released)
            };
            if released {
                return Err("the compositor released the pane".to_owned());
            }
            quiet = if pending { 0 } else { quiet + 1 };
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("circulating while settling: {error}"))?;
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn commit(&mut self, index: usize, record: bool) -> Result<(), String> {
        self.state.commit_next(
            &self.qh,
            &mut self.event_queue,
            &self.session.log,
            index,
            record,
        )
    }

    fn press(&self, pressed: bool) -> Result<(), String> {
        self.session
            .commands
            .send(Command::Input {
                pane: PaneId(1),
                event: PaneInput::Key {
                    code: 30,
                    pressed,
                    modifiers: 0,
                    held: Vec::new(),
                },
            })
            .map_err(|error| format!("sending input: {error}"))
    }

    fn measure_input(&mut self, iterations: usize) -> Result<Option<Row>, String> {
        let deadline = Instant::now() + TIMEOUT;
        while !self.state.keyboard_focus {
            if Instant::now() > deadline {
                return Err("timed out waiting for keyboard focus".to_owned());
            }
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("waiting for focus: {error}"))?;
            thread::sleep(Duration::from_millis(1));
        }
        for _ in 0..10 {
            self.press(true)?;
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("warming up input: {error}"))?;
            self.press(false)?;
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("warming up input release: {error}"))?;
        }
        let started = Instant::now();
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            self.state.last_key = None;
            let sample = Instant::now();
            self.press(true)?;
            while self.state.last_key.is_none() {
                self.event_queue
                    .blocking_dispatch(&mut self.state)
                    .map_err(|error| format!("waiting for key: {error}"))?;
            }
            samples.push(sample.elapsed());
            self.press(false)?;
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("waiting for key release: {error}"))?;
        }
        let elapsed = started.elapsed();
        Ok(Row::new(
            "interactive/key/roundtrip",
            iterations,
            samples,
            elapsed,
            Rate::Ops,
        ))
    }
}

fn video(config: Config) -> Result<Option<Row>, String> {
    let mut client = Client::connect(&config)?;
    client.settle()?;
    for index in 0..config.warmup {
        client.commit(index, false)?;
    }
    client.wait_queue_empty()?;
    client.settle()?;
    let mut log = client.session.log.lock().unwrap();
    log.latencies.clear();
    drop(log);
    let started = Instant::now();
    for index in config.warmup..config.warmup + config.frames {
        client.commit(index, true)?;
    }
    client.wait_queue_empty()?;
    let elapsed = started.elapsed();
    let mut log = client.session.log.lock().unwrap();
    let samples = std::mem::take(&mut log.latencies);
    drop(log);
    let count = samples.len();
    Ok(Row::new(config.label, count, samples, elapsed, Rate::Fps))
}

fn input(frame_count: usize, quick: bool) -> Result<Option<Row>, String> {
    let config = Config {
        label: "interactive/key/roundtrip".to_owned(),
        width: 1920,
        height: 1080,
        kind: Kind::Shm,
        pattern: Pattern::Cell,
        frames: frame_count,
        warmup: 20,
        input: true,
    };
    let mut client = Client::connect(&config)?;
    client.measure_input(if quick { 200 } else { 1000 })
}

pub struct Settings {
    pub quick: bool,
    pub frames: Option<usize>,
}

pub fn run(settings: &Settings) -> Vec<Row> {
    let normal = if settings.quick { 90 } else { 240 };
    let frames = settings.frames.unwrap_or(normal);
    let four_k = settings
        .frames
        .unwrap_or(if settings.quick { 45 } else { 120 });
    let warmup = if settings.quick { 12 } else { 24 };
    let mut rows = Vec::new();
    let mut run = |config: Config| match video(config) {
        Ok(Some(row)) => rows.push(row),
        Ok(None) => {}
        Err(error) => eprintln!("pipeline skipped: {error}"),
    };

    for (label, kind) in [
        ("pipeline/commit-to-terminal/1080p/noise/shm", Kind::Shm),
        ("pipeline/commit-to-terminal/1080p/video/shm", Kind::Shm),
        ("pipeline/commit-to-terminal/1080p/cell/shm", Kind::Shm),
        ("pipeline/commit-to-terminal/1080p/band/shm", Kind::Shm),
    ] {
        run(Config {
            label: label.to_owned(),
            width: 1920,
            height: 1080,
            kind,
            pattern: if label.contains("noise") {
                Pattern::Noise
            } else if label.contains("video") {
                Pattern::Video
            } else if label.contains("band") {
                Pattern::Band
            } else {
                Pattern::Cell
            },
            frames,
            warmup,
            input: false,
        });
    }
    run(Config {
        label: "pipeline/callback-pacing/320x180/shm".to_owned(),
        width: 320,
        height: 180,
        kind: Kind::Shm,
        pattern: Pattern::Noise,
        frames,
        warmup,
        input: false,
    });
    run(Config {
        label: "pipeline/commit-to-terminal/2160p/noise/shm".to_owned(),
        width: 3840,
        height: 2160,
        kind: Kind::Shm,
        pattern: Pattern::Noise,
        frames: four_k,
        warmup,
        input: false,
    });
    if !settings.quick {
        for (label, width, height, pattern, count) in [
            (
                "pipeline/commit-to-terminal/1080p/noise/dmabuf",
                1920,
                1080,
                Pattern::Noise,
                frames,
            ),
            (
                "pipeline/commit-to-terminal/2160p/noise/dmabuf",
                3840,
                2160,
                Pattern::Noise,
                four_k,
            ),
        ] {
            run(Config {
                label: label.to_owned(),
                width,
                height,
                kind: Kind::DmaBuf,
                pattern,
                frames: count,
                warmup,
                input: false,
            });
        }
    }
    match input(frames, settings.quick) {
        Ok(Some(row)) => rows.push(row),
        Ok(None) => {}
        Err(error) => eprintln!("input pipeline skipped: {error}"),
    }
    rows
}
