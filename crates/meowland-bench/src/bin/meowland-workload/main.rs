//! Deterministic SHM Wayland application controlled through a benchmark-only
//! Unix socket.

mod dispatch;

use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::AsFd,
        unix::{fs::FileExt as _, net::UnixStream},
    },
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use meowland_bench::{
    control::{self, Command, Event, EventKind},
    pattern::{APP_ID, Pattern, Rect},
};
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, memfd_create},
};
use wayland_client::{
    Connection, EventQueue, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;

/// Evdev code of the `a` key.
const KEY_A: u32 = 30;

struct Buffer {
    proxy: wl_buffer::WlBuffer,
    small: wl_buffer::WlBuffer,
    file: File,
    released: bool,
    /// Whether every pixel has been painted, so later frames of a pattern
    /// with bounded damage only need to repaint that damage.
    painted: bool,
    /// Frame the compositor holds this buffer for.
    frame: Option<u64>,
}

impl Buffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        (width, height): (usize, usize),
        index: usize,
    ) -> anyhow::Result<Self> {
        let file = File::from(memfd_create("meowland-workload", MemfdFlags::CLOEXEC)?);
        let length = width * height * 4;
        file.set_len(u64::try_from(length)?)?;
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let create = |width, height| -> anyhow::Result<_> {
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
            painted: false,
            frame: None,
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

impl State {
    fn event(&mut self, kind: EventKind, id: u64) {
        let event = Event {
            kind,
            id,
            at: control::now(),
        };
        if let Err(error) = writeln!(self.control, "{event}") {
            self.error = Some(error.to_string());
        }
    }

    fn input(&mut self, key: u32) {
        if key == KEY_A
            && let Some(id) = self.expectation.take()
        {
            self.event(EventKind::Input, id);
            self.pending = Some((id, id));
        }
    }

    fn release(&mut self, index: usize) {
        let buffer = &mut self.buffers[index];
        buffer.released = true;
        if let Some(frame) = buffer.frame.take() {
            self.event(EventKind::Release, frame);
        }
    }

    fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Commit(id) => self.pending = Some((id, id)),
            Command::Repeat { frame, content } => self.pending = Some((frame, content)),
            Command::Arm(id) => {
                self.expectation = Some(id);
                self.event(EventKind::Armed, id);
            }
            Command::Stop => return false,
        }
        true
    }

    fn draw(&mut self) -> anyhow::Result<()> {
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
        let (width, height) = self.pattern.surface(self.width, self.height, content);
        let buffer = &mut self.buffers[index];
        if let Some(rectangles) = self.pattern.damage().filter(|_| buffer.painted) {
            for rectangle in rectangles {
                paint(&mut self.pixels, width, self.pattern, content, rectangle);
                let (x, y, w, h) = rectangle;
                for row in y..y + h {
                    let start = (row * width + x) * 4;
                    let end = start + w * 4;
                    buffer
                        .file
                        .write_all_at(&self.pixels[start..end], u64::try_from(start)?)?;
                }
            }
        } else {
            paint(
                &mut self.pixels,
                width,
                self.pattern,
                content,
                (0, 0, width, height),
            );
            buffer
                .file
                .write_all_at(&self.pixels[..width * height * 4], 0)?;
            buffer.painted = true;
        }
        buffer.released = false;
        buffer.frame = Some(id);
        let proxy = if (width, height) == (self.width, self.height) {
            &buffer.proxy
        } else {
            &buffer.small
        };
        self.surface.attach(Some(proxy), 0, 0);
        match self.pattern.damage().filter(|_| !initial) {
            Some(rectangles) => {
                for (x, y, w, h) in rectangles {
                    self.surface.damage_buffer(
                        i32::try_from(x)?,
                        i32::try_from(y)?,
                        i32::try_from(w)?,
                        i32::try_from(h)?,
                    );
                }
            }
            None => {
                self.surface
                    .damage_buffer(0, 0, i32::try_from(width)?, i32::try_from(height)?);
            }
        }
        let at = control::now();
        self.surface.commit();
        let event = Event {
            kind: EventKind::Commit,
            id,
            at,
        };
        writeln!(self.control, "{event}")?;
        Ok(())
    }
}

/// Paint `rectangle` of `frame` into XRGB8888 `pixels` holding a surface
/// `width` pixels wide.
fn paint(pixels: &mut [u8], width: usize, pattern: Pattern, frame: u64, rectangle: Rect) {
    let (x, y, w, h) = rectangle;
    for row in y..y + h {
        let start = (row * width + x) * 4;
        let line = &mut pixels[start..start + w * 4];
        for (column, pixel) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let [red, green, blue] = pattern.surface_pixel(x + column, row, width, frame);
            *pixel = [blue, green, red, 255];
        }
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [control, width, height, pattern] = args.as_slice() else {
        bail!("usage: meowland-workload CONTROL_SOCKET WIDTH HEIGHT full|sparse|resize");
    };
    let width: usize = width.parse()?;
    let height: usize = height.parse()?;
    ensure!(
        (160..=7680).contains(&width) && (120..=4320).contains(&height),
        "workload dimensions must be 160x120 through 7680x4320"
    );
    let pattern: Pattern = pattern.parse()?;
    let control = UnixStream::connect(control).context("connecting to the harness")?;
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
    toplevel.set_app_id(APP_ID.into());
    toplevel.set_title("Meowland benchmark workload".into());
    surface.commit();
    let buffers = (0..3)
        .map(|index| Buffer::new(&shm, &qh, (width, height), index))
        .collect::<anyhow::Result<_>>()?;
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
    state.event(EventKind::Pid, u64::from(std::process::id()));
    event_loop(&connection, queue, state)
}

fn event_loop(
    connection: &Connection,
    mut queue: EventQueue<State>,
    mut state: State,
) -> anyhow::Result<()> {
    let mut pending = String::new();
    let mut bytes = [0; 4096];
    let timeout = Duration::from_millis(100).try_into()?;
    loop {
        queue.dispatch_pending(&mut state)?;
        if let Some(error) = state.error.take() {
            bail!(error);
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
            bail!("Wayland disconnected");
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
                if !state.command(line.parse()?) {
                    return Ok(());
                }
            }
            ensure!(pending.len() <= 4096, "oversized control command");
        }
    }
}

fn main() -> anyhow::Result<()> {
    run()
}
