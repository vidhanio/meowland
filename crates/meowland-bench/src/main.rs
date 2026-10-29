//! Deterministic SHM Wayland application controlled through a benchmark-only
//! Unix socket.

mod dispatch;
mod fixture;

use std::{
    fs::File,
    io::{Read, Seek, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    time::Duration,
};

use fixture::Pattern;
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, memfd_create},
    time::{ClockId, clock_gettime},
};
use wayland_client::{
    Connection, EventQueue, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Buffer {
    proxy: wl_buffer::WlBuffer,
    small: wl_buffer::WlBuffer,
    file: File,
    released: bool,
}

impl Buffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        (width, height): (usize, usize),
        index: usize,
    ) -> Result<Self> {
        let file = File::from(memfd_create("meowland-workload", MemfdFlags::CLOEXEC)?);
        let length = width * height * 4;
        file.set_len(u64::try_from(length)?)?;
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let create = |width, height| -> Result<_> {
            Ok(pool.create_buffer(
                0,
                i32::try_from(width)?,
                i32::try_from(height)?,
                i32::try_from(width * 4)?,
                wl_shm::Format::Xrgb8888,
                qh,
                index,
            ))
        };
        let proxy = create(width, height)?;
        let small = create(width / 2, height / 2)?;
        pool.destroy();
        Ok(Self {
            proxy,
            small,
            file,
            released: true,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    WaitingConfigure,
    Initial,
    Running,
}

struct State {
    width: usize,
    height: usize,
    pattern: Pattern,
    pixels: Vec<u8>,
    buffers: Vec<Buffer>,
    surface: wl_surface::WlSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    phase: Phase,
    ready: bool,
    pending: Option<(u64, u64)>,
    expectation: Option<u64>,
    control: UnixStream,
    error: Option<String>,
}

fn now() -> i64 {
    let time = clock_gettime(ClockId::Monotonic);
    time.tv_sec * 1_000_000_000 + time.tv_nsec
}

impl State {
    fn event(&mut self, kind: &str, id: u64) {
        if let Err(error) = writeln!(self.control, "{kind} {id} {}", now()) {
            self.error = Some(error.to_string());
        }
    }

    fn input(&mut self, key: u32) {
        if key == 30
            && let Some(id) = self.expectation.take()
        {
            self.event("input", id);
            self.pending = Some((id, id));
        }
    }

    fn command(&mut self, line: &str) -> Result<bool> {
        let mut words = line.split_whitespace();
        let kind = words.next().ok_or("empty command")?;
        if kind == "stop" {
            return Ok(false);
        }
        let id: u64 = words.next().ok_or("missing command ID")?.parse()?;
        match kind {
            "commit" => self.pending = Some((id, id)),
            "repeat" => {
                let content = words.next().ok_or("missing repeat content ID")?.parse()?;
                self.pending = Some((id, content));
            }
            "arm" => {
                self.expectation = Some(id);
                self.event("armed", id);
            }
            _ => return Err("unknown command".into()),
        }
        Ok(true)
    }

    fn draw(&mut self) -> Result<()> {
        if self.phase == Phase::WaitingConfigure {
            return Ok(());
        }
        let Some(index) = self.buffers.iter().position(|buffer| buffer.released) else {
            return Ok(());
        };
        let initial = self.phase == Phase::Initial;
        let (id, content) = if initial {
            self.phase = Phase::Running;
            (0, 0)
        } else if let Some(pending) = self.pending.take() {
            pending
        } else {
            return Ok(());
        };
        let small = matches!(self.pattern, Pattern::Resize) && id & 1 != 0;
        let (width, height) = if small {
            (self.width / 2, self.height / 2)
        } else {
            (self.width, self.height)
        };
        let pixels = &mut self.pixels[..width * height * 4];
        fixture::paint(pixels, width, self.pattern, content);
        let buffer = &mut self.buffers[index];
        buffer.file.rewind()?;
        buffer.file.write_all(pixels)?;
        buffer.released = false;
        let proxy = if small { &buffer.small } else { &buffer.proxy };
        self.surface.attach(Some(proxy), 0, 0);
        if initial || !matches!(self.pattern, Pattern::Sparse) {
            self.surface
                .damage_buffer(0, 0, i32::try_from(width)?, i32::try_from(height)?);
        } else {
            self.surface.damage_buffer(0, 0, 16, 1);
            self.surface.damage_buffer(80, 100, 40, 20);
        }
        let at = now();
        self.surface.commit();
        writeln!(self.control, "commit {id} {at}")?;
        Ok(())
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [control, width, height, pattern] = args.as_slice() else {
        return Err(
            "usage: meowland-workload CONTROL_SOCKET WIDTH HEIGHT full|sparse|resize".into(),
        );
    };
    let width: usize = width.parse()?;
    let height: usize = height.parse()?;
    if !(160..=7680).contains(&width) || !(120..=4320).contains(&height) {
        return Err("workload dimensions must be 160x120 through 7680x4320".into());
    }
    let pattern = match pattern.as_str() {
        "full" => Pattern::Full,
        "sparse" => Pattern::Sparse,
        "resize" => Pattern::Resize,
        _ => return Err("pattern must be full, sparse, or resize".into()),
    };
    let control = UnixStream::connect(control)?;
    control.set_read_timeout(Some(Duration::from_secs(5)))?;
    control.set_write_timeout(Some(Duration::from_secs(5)))?;
    let connection = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ())?;
    let _seat: wl_seat::WlSeat = globals.bind(&qh, 5..=5, ())?;
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id(fixture::APP_ID.into());
    toplevel.set_title("Meowland benchmark workload".into());
    surface.commit();
    let buffers = (0..3)
        .map(|index| Buffer::new(&shm, &qh, (width, height), index))
        .collect::<Result<_>>()?;
    let mut state = State {
        width,
        height,
        pattern,
        pixels: vec![0; width * height * 4],
        buffers,
        surface,
        keyboard: None,
        phase: Phase::WaitingConfigure,
        ready: false,
        pending: None,
        expectation: None,
        control,
        error: None,
    };
    state.event("pid", u64::from(std::process::id()));
    event_loop(&connection, queue, state)
}

fn event_loop(
    connection: &Connection,
    mut queue: EventQueue<State>,
    mut state: State,
) -> Result<()> {
    let mut pending = String::new();
    let mut bytes = [0; 4096];
    let timeout = Duration::from_millis(100).try_into()?;
    loop {
        queue.dispatch_pending(&mut state)?;
        if let Some(error) = state.error.take() {
            return Err(error.into());
        }
        state.draw()?;
        queue.flush()?;
        let Some(read) = connection.prepare_read() else {
            continue;
        };
        let (wayland, command) = {
            let mut descriptors = [
                PollFd::new(connection, PollFlags::IN),
                PollFd::new(&state.control, PollFlags::IN),
            ];
            poll(&mut descriptors, Some(&timeout))?;
            (descriptors[0].revents(), descriptors[1].revents())
        };
        if wayland.intersects(PollFlags::ERR | PollFlags::HUP) {
            return Err("Wayland disconnected".into());
        }
        if wayland.contains(PollFlags::IN) {
            read.read()?;
        } else {
            drop(read);
        }
        if !command.is_empty() {
            let count = state.control.read(&mut bytes)?;
            if count == 0 {
                return Ok(());
            }
            pending.push_str(std::str::from_utf8(&bytes[..count])?);
            while let Some(end) = pending.find('\n') {
                let line: String = pending.drain(..=end).collect();
                if !state.command(&line)? {
                    return Ok(());
                }
            }
            if pending.len() > 4096 {
                return Err("oversized control command".into());
            }
        }
    }
}

fn main() -> Result<()> {
    run()
}
