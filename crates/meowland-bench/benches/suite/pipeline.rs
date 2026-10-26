use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::Device as ControlDevice;
use drm::control::dumbbuffer::DumbBuffer;
use memmap2::MmapMut;
use meowland_compositor::{Command, PaneId};
use meowland_protocol::{Input as PaneInput, PaneAction, Show};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::RunLength;
use crate::session::{CommitLog, Sampling, Session, TIMEOUT};
use crate::stats::{Rate, Row};

mod dispatch;

const BUFFERS: usize = 3;
const APP_ID: &str = "meowland-bench";

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Disabled,
    Keyboard,
}

struct Config {
    label: String,
    width: u32,
    height: u32,
    kind: Kind,
    pattern: Pattern,
    frames: usize,
    warmup: usize,
    input: InputMode,
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
        let row = usize::try_from(self.width).expect("benchmark width fits usize") * 4;
        let height = usize::try_from(self.height).expect("benchmark height fits usize");
        if self.stride == row {
            self.mapping[..row * height].copy_from_slice(frame);
        } else {
            for y in 0..height {
                let source = &frame[y * row..(y + 1) * row];
                let dest = &mut self.mapping[y * self.stride..y * self.stride + row];
                dest.copy_from_slice(source);
            }
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, value: u8) {
        for row in y..y + height {
            let start = usize::try_from(row).expect("rectangle row fits usize") * self.stride
                + usize::try_from(x).expect("rectangle column fits usize") * 4;
            let bytes = usize::try_from(width).expect("rectangle width fits usize") * 4;
            self.mapping[start..start + bytes].fill(value);
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
        let len = usize::try_from(config.width).expect("benchmark width fits usize")
            * usize::try_from(config.height).expect("benchmark height fits usize")
            * 4;
        match config.pattern {
            Pattern::Noise => Self::Full(
                (0..4)
                    .map(|index| noise_frame(config.width, config.height, index + 1))
                    .collect(),
            ),
            Pattern::Video => Self::Full(
                (0..4)
                    .map(|index| video_frame(config.width, config.height, index))
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
                let x =
                    (u32::try_from(index).expect("benchmark frame index fits u32") * cell.0 * 2)
                        % (buffer.width - cell.0);
                let y = buffer.height / 2;
                buffer.fill_rect(x, y, cell.0, cell.1, 0xab);
                (x, y, cell.0, cell.1)
            }
            Self::Band { base, cell_height } => {
                buffer.upload(base);
                let y = (u32::try_from(index).expect("benchmark frame index fits u32")
                    * cell_height)
                    % (buffer.height - cell_height);
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
    let pixels = usize::try_from(width).expect("benchmark width fits usize")
        * usize::try_from(height).expect("benchmark height fits usize");
    let mut frame = Vec::with_capacity(pixels * 4);
    for _ in 0..pixels {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let [red, green, blue, _] = state.to_le_bytes();
        frame.extend_from_slice(&[red, green, blue, 0xff]);
    }
    frame
}

fn video_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
    let pixels = usize::try_from(width).expect("benchmark width fits usize")
        * usize::try_from(height).expect("benchmark height fits usize");
    let mut frame = Vec::with_capacity(pixels * 4);
    for y in 0..height {
        for x in 0..width {
            let noise = (x.wrapping_mul(17) ^ y.wrapping_mul(31) ^ index).wrapping_mul(0x9e37_79b9);
            frame.extend_from_slice(&[
                ((x * 255 / width) ^ (index * 7)).to_le_bytes()[0],
                ((y * 255 / height) ^ (index * 11)).to_le_bytes()[0],
                noise.to_be_bytes()[0],
                0xff,
            ]);
        }
    }
    frame
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
    card: Option<Card>,
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
        input: InputMode,
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
        if input == InputMode::Keyboard {
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
        if self.surface.is_none() {
            return Err("surface was not created".to_owned());
        }
        match self.kind {
            Kind::Shm => {
                let shm = self
                    .shm
                    .as_ref()
                    .ok_or_else(|| "wl_shm was not bound".to_owned())?;
                let stride = usize::try_from(self.width).map_err(|error| error.to_string())? * 4;
                let len =
                    stride * usize::try_from(self.height).map_err(|error| error.to_string())?;
                for _ in 0..BUFFERS {
                    let fd = memfd_create("meowland-bench", MemfdFlags::CLOEXEC)
                        .map_err(|error| format!("memfd_create: {error}"))?;
                    ftruncate(&fd, u64::try_from(len).map_err(|error| error.to_string())?)
                        .map_err(|error| format!("ftruncate: {error}"))?;
                    // SAFETY: This process owns and sizes the memfd, retains
                    // the mapping, and writes only after
                    // the compositor releases the corresponding buffer.
                    let mapping = unsafe { MmapMut::map_mut(&fd) }
                        .map_err(|error| format!("mmap: {error}"))?;
                    let pool = shm.create_pool(
                        fd.as_fd(),
                        i32::try_from(len).map_err(|error| error.to_string())?,
                        qh,
                        (),
                    );
                    let released = Arc::new(AtomicBool::new(true));
                    let buffer = pool.create_buffer(
                        0,
                        i32::try_from(self.width).map_err(|error| error.to_string())?,
                        i32::try_from(self.height).map_err(|error| error.to_string())?,
                        i32::try_from(stride).map_err(|error| error.to_string())?,
                        wl_shm::Format::Xrgb8888,
                        qh,
                        BufferState(Arc::clone(&released)),
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
                let card = open_card()?;
                for _ in 0..BUFFERS {
                    let dumb = card
                        .create_dumb_buffer((self.width, self.height), DrmFourcc::Xrgb8888, 32)
                        .map_err(|error| format!("create_dumb_buffer: {error}"))?;
                    let stride =
                        usize::try_from(dumb.pitch()).map_err(|error| error.to_string())?;
                    let fd = card
                        .buffer_to_prime_fd(dumb.handle(), drm::RDWR)
                        .map_err(|error| format!("buffer_to_prime_fd: {error}"))?;
                    // SAFETY: The exported dumb buffer has a stable allocation
                    // owned by this client; writes occur
                    // only between compositor releases and commits.
                    let mapping = unsafe { MmapMut::map_mut(&fd) }
                        .map_err(|error| format!("mapping dmabuf: {error}"))?;
                    let params = dmabuf.create_params(qh, ());
                    params.add(fd.as_fd(), 0, 0, dumb.pitch(), 0, 0);
                    let released = Arc::new(AtomicBool::new(true));
                    #[expect(
                        clippy::as_conversions,
                        reason = "DrmFourcc is repr(u32), matching the Wayland format argument"
                    )]
                    let format = DrmFourcc::Xrgb8888 as u32;
                    let buffer = params.create_immed(
                        i32::try_from(self.width).map_err(|error| error.to_string())?,
                        i32::try_from(self.height).map_err(|error| error.to_string())?,
                        format,
                        zwp_linux_buffer_params_v1::Flags::empty(),
                        qh,
                        BufferState(Arc::clone(&released)),
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
        self.attach_initial();
        Ok(())
    }

    fn attach_initial(&self) {
        if self.configured
            && let (Some(surface), Some(buffer)) = (&self.surface, self.buffers.first())
        {
            surface.attach(Some(&buffer.buffer), 0, 0);
            surface.damage_buffer(
                0,
                0,
                i32::try_from(self.width).expect("benchmark width fits Wayland coordinates"),
                i32::try_from(self.height).expect("benchmark height fits Wayland coordinates"),
            );
            surface.commit();
        }
    }

    fn commit_next(
        &mut self,
        qh: &QueueHandle<Self>,
        event_queue: &mut EventQueue<Self>,
        log: &Mutex<CommitLog>,
        index: usize,
        sampling: Sampling,
    ) -> Result<(), String> {
        let buffer_index = self.next_buffer;
        let released = Arc::clone(&self.buffers[buffer_index].released);
        while !released.swap(false, Ordering::AcqRel) {
            event_queue
                .blocking_dispatch(self)
                .map_err(|error| format!("waiting for buffer release: {error}"))?;
        }
        let buffer = &mut self.buffers[buffer_index];
        let damage = self.prepared.prepare(index, buffer);
        let callback = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        log.lock()
            .expect("benchmark commit log lock poisoned")
            .queue
            .push_back((started, sampling));
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| "surface was not created".to_owned())?;
        surface.frame(qh, Arc::clone(&callback));
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

struct Client {
    state: State,
    event_queue: EventQueue<State>,
    qh: QueueHandle<State>,
    session: Session,
}

impl Client {
    fn connect(config: &Config) -> Result<Self, String> {
        let session = Session::new()?;
        let socket = session.socket();
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
        while self
            .session
            .log
            .lock()
            .expect("benchmark commit log lock poisoned")
            .presented
            < target
        {
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
                let log = self
                    .session
                    .log
                    .lock()
                    .expect("benchmark commit log lock poisoned");
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
                let log = self
                    .session
                    .log
                    .lock()
                    .expect("benchmark commit log lock poisoned");
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

    fn commit(&mut self, index: usize, sampling: Sampling) -> Result<(), String> {
        self.state.commit_next(
            &self.qh,
            &mut self.event_queue,
            &self.session.log,
            index,
            sampling,
        )
    }

    fn press(&self, transition: wl_keyboard::KeyState) -> Result<(), String> {
        self.session
            .commands
            .send(Command::Pane {
                pane: PaneId(1),
                action: PaneAction::Input(PaneInput::Key {
                    code: 30,
                    pressed: transition == wl_keyboard::KeyState::Pressed,
                    modifiers: 0,
                    held: Vec::new(),
                }),
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
            self.press(wl_keyboard::KeyState::Pressed)?;
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("warming up input: {error}"))?;
            self.press(wl_keyboard::KeyState::Released)?;
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("warming up input release: {error}"))?;
        }
        let started = Instant::now();
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            self.state.last_key = None;
            let sample = Instant::now();
            self.press(wl_keyboard::KeyState::Pressed)?;
            while self.state.last_key.is_none() {
                self.event_queue
                    .blocking_dispatch(&mut self.state)
                    .map_err(|error| format!("waiting for key: {error}"))?;
            }
            samples.push(sample.elapsed());
            self.press(wl_keyboard::KeyState::Released)?;
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
    let result = measure_video(&mut client, config);
    client.session.close()?;
    result
}

fn measure_video(client: &mut Client, config: Config) -> Result<Option<Row>, String> {
    client.settle()?;
    for index in 0..config.warmup {
        client.commit(index, Sampling::Warmup)?;
    }
    client.wait_queue_empty()?;
    client.settle()?;
    let mut log = client
        .session
        .log
        .lock()
        .expect("benchmark commit log lock poisoned");
    log.latencies.clear();
    drop(log);
    let started = Instant::now();
    for index in config.warmup..config.warmup + config.frames {
        client.commit(index, Sampling::Record)?;
    }
    client.wait_queue_empty()?;
    let elapsed = started.elapsed();
    let mut log = client
        .session
        .log
        .lock()
        .expect("benchmark commit log lock poisoned");
    let samples = std::mem::take(&mut log.latencies);
    drop(log);
    let count = samples.len();
    Ok(Row::new(config.label, count, samples, elapsed, Rate::Fps))
}

fn input(frame_count: usize, length: RunLength) -> Result<Option<Row>, String> {
    let config = Config {
        label: "interactive/key/roundtrip".to_owned(),
        width: 1920,
        height: 1080,
        kind: Kind::Shm,
        pattern: Pattern::Cell,
        frames: frame_count,
        warmup: 20,
        input: InputMode::Keyboard,
    };
    let mut client = Client::connect(&config)?;
    let result = client.measure_input(if length == RunLength::Quick {
        200
    } else {
        1000
    });
    client.session.close()?;
    result
}

pub(crate) struct Settings {
    pub length: RunLength,
    pub frames: Option<usize>,
    pub filter: Option<String>,
}

impl Settings {
    fn includes(&self, label: &str) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|filter| label.contains(filter))
    }
}

fn record_result(result: Result<Option<Row>, String>, rows: &mut Vec<Row>) {
    match result {
        Ok(row) => rows.extend(row),
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "pipeline skipped: {error}");
        }
    }
}

pub(crate) fn run(settings: &Settings) -> Vec<Row> {
    let (normal, large, warmup) = match settings.length {
        RunLength::Quick => (90, 45, 12),
        RunLength::Full => (240, 120, 24),
    };
    let frames = settings.frames.unwrap_or(normal);
    let four_k = settings.frames.unwrap_or(large);
    let mut rows = Vec::new();
    let mut run = |config: Config| {
        if !settings.includes(&config.label) {
            return;
        }
        record_result(video(config), &mut rows);
    };

    for (label, pattern) in [
        (
            "pipeline/commit-to-terminal/1080p/noise/shm",
            Pattern::Noise,
        ),
        (
            "pipeline/commit-to-terminal/1080p/video/shm",
            Pattern::Video,
        ),
        ("pipeline/commit-to-terminal/1080p/cell/shm", Pattern::Cell),
        ("pipeline/commit-to-terminal/1080p/band/shm", Pattern::Band),
    ] {
        run(Config {
            label: label.to_owned(),
            width: 1920,
            height: 1080,
            kind: Kind::Shm,
            pattern,
            frames,
            warmup,
            input: InputMode::Disabled,
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
        input: InputMode::Disabled,
    });
    run(Config {
        label: "pipeline/commit-to-terminal/2160p/noise/shm".to_owned(),
        width: 3840,
        height: 2160,
        kind: Kind::Shm,
        pattern: Pattern::Noise,
        frames: four_k,
        warmup,
        input: InputMode::Disabled,
    });
    if settings.length == RunLength::Full {
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
                input: InputMode::Disabled,
            });
        }
    }
    if settings.includes("interactive/key/roundtrip") {
        record_result(input(frames, settings.length), &mut rows);
    }
    rows
}
