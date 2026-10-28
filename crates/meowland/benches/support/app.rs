//! A real Wayland workload client with explicit input expectations and frame
//! identities.

use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, Write},
    net::Shutdown,
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, memfd_create},
};
use wayland_client::{
    Connection, EventQueue,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::{
    config::Size,
    fixture::{APP_ID, Fixture, LOAD_BIT, Pattern},
    record::Event,
};
use crate::{Result, fail};

mod dispatch;

const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 120);

#[derive(Clone, Copy, Debug)]
pub(crate) enum Input {
    Key(u32),
    Release(u32),
    Pointer(u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Command {
    Commit(u64),
    Arm { id: u64, input: Input },
    StartLoad,
    StopLoad,
    Stop,
}

pub(crate) struct App {
    commands: mpsc::SyncSender<Command>,
    wake: UnixStream,
    socket: UnixStream,
    thread: Option<JoinHandle<()>>,
}

struct Buffer {
    proxy: wl_buffer::WlBuffer,
    file: File,
    released: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    WaitingConfigure,
    Initial,
    Running,
}

struct State {
    fixture: Fixture,
    pixels: Vec<u8>,
    buffers: Vec<Buffer>,
    surface: wl_surface::WlSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
    _xdg: xdg_surface::XdgSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    phase: Phase,
    ready: bool,
    stopped: bool,
    pending: VecDeque<u64>,
    expectation: Option<(u64, Input)>,
    response: Option<u64>,
    load: bool,
    sequence: u64,
    next_frame: Instant,
    events: mpsc::Sender<Event>,
    error: Option<String>,
}

impl State {
    fn event(&mut self, event: Event) {
        if self.events.send(event).is_err() {
            self.stopped = true;
        }
    }

    fn input(&mut self, input: Input) {
        let Some((id, expected)) = self.expectation else {
            return;
        };
        let matches = match (expected, input) {
            (Input::Key(a), Input::Key(b)) | (Input::Release(a), Input::Release(b)) => a == b,
            (Input::Pointer(ax, ay), Input::Pointer(bx, by)) => ax == bx && ay == by,
            _ => false,
        };
        if !matches {
            return;
        }
        self.expectation = None;
        if matches!(input, Input::Release(_)) {
            self.event(Event::Released(id));
        } else {
            self.event(Event::Input {
                id,
                at: Instant::now(),
            });
            self.response = Some(id);
            self.pending.push_back(id);
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Commit(id) => {
                self.response = None;
                self.pending.push_back(id);
            }
            Command::Arm { id, input } => {
                if !matches!(input, Input::Release(_)) {
                    self.response = None;
                }
                self.expectation = Some((id, input));
                self.event(Event::Armed(id));
            }
            Command::StartLoad => {
                self.load = true;
                self.next_frame = Instant::now();
            }
            Command::StopLoad => self.load = false,
            Command::Stop => self.stopped = true,
        }
    }

    fn commit(&mut self, id: u64, index: usize) -> Result<()> {
        self.sequence += 1;
        self.fixture.paint(id, self.sequence, &mut self.pixels);
        let buffer = &mut self.buffers[index];
        buffer.file.rewind()?;
        buffer.file.write_all(&self.pixels)?;
        buffer.released = false;
        self.surface.attach(Some(&buffer.proxy), 0, 0);
        for (x, y, width, height) in self.fixture.damage() {
            self.surface.damage_buffer(
                i32::try_from(x)?,
                i32::try_from(y)?,
                i32::try_from(width)?,
                i32::try_from(height)?,
            );
        }
        let at = Instant::now();
        self.surface.commit();
        if id & LOAD_BIT == 0 {
            self.event(Event::Committed { id, at });
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if self.phase == Phase::WaitingConfigure {
            return Ok(());
        }
        let Some(index) = self.buffers.iter().position(|buffer| buffer.released) else {
            return Ok(());
        };
        if self.phase == Phase::Initial {
            self.phase = Phase::Running;
            return self.commit(0, index);
        }
        if let Some(id) = self.pending.pop_front() {
            return self.commit(id, index);
        }
        if self.load && Instant::now() >= self.next_frame {
            self.next_frame = Instant::now() + FRAME_PERIOD;
            return self.commit(
                self.response.unwrap_or(LOAD_BIT | (self.sequence + 1)),
                index,
            );
        }
        Ok(())
    }
}

fn connect(
    socket: UnixStream,
    size: Size,
    pattern: Pattern,
    events: mpsc::Sender<Event>,
) -> Result<(Connection, EventQueue<State>, State)> {
    let connection = Connection::from_socket(socket)?;
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
    toplevel.set_title("Meowland end-to-end workload".into());
    surface.commit();
    let mut buffers = Vec::new();
    for index in 0..3 {
        let file = File::from(memfd_create("meowland-e2e", MemfdFlags::CLOEXEC)?);
        let length = size.pixels() * 4;
        file.set_len(u64::try_from(length)?)?;
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, &qh, ());
        let proxy = pool.create_buffer(
            0,
            i32::try_from(size.width)?,
            i32::try_from(size.height)?,
            i32::try_from(size.width * 4)?,
            wl_shm::Format::Xrgb8888,
            &qh,
            index,
        );
        pool.destroy();
        buffers.push(Buffer {
            proxy,
            file,
            released: true,
        });
    }
    let state = State {
        fixture: Fixture::new(size, pattern),
        pixels: vec![0; size.pixels() * 4],
        buffers,
        surface,
        _toplevel: toplevel,
        _xdg: xdg,
        keyboard: None,
        pointer: None,
        phase: Phase::WaitingConfigure,
        ready: false,
        stopped: false,
        pending: VecDeque::new(),
        expectation: None,
        response: None,
        load: false,
        sequence: 0,
        next_frame: Instant::now(),
        events,
        error: None,
    };
    Ok((connection, queue, state))
}

fn run(
    socket: UnixStream,
    size: Size,
    pattern: Pattern,
    events: mpsc::Sender<Event>,
    commands: &mpsc::Receiver<Command>,
    mut wake: UnixStream,
) -> Result<()> {
    let (connection, mut queue, mut state) = connect(socket, size, pattern, events)?;
    let mut bytes = [0; 128];
    while !state.stopped {
        queue.dispatch_pending(&mut state)?;
        for command in commands.try_iter() {
            state.command(command);
        }
        if state.stopped {
            break;
        }
        if let Some(error) = state.error.take() {
            return Err(fail(error));
        }
        state.draw()?;
        queue.flush()?;
        let Some(read) = connection.prepare_read() else {
            continue;
        };
        let duration = if state.load {
            state
                .next_frame
                .saturating_duration_since(Instant::now())
                .max(Duration::from_micros(100))
        } else {
            Duration::from_secs(1)
        };
        let timeout = duration.try_into()?;
        let (wayland, command) = {
            let mut descriptors = [
                PollFd::new(&connection, PollFlags::IN),
                PollFd::new(&wake, PollFlags::IN),
            ];
            poll(&mut descriptors, Some(&timeout))?;
            (descriptors[0].revents(), descriptors[1].revents())
        };
        if wayland.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
            return Err(fail("Wayland connection closed"));
        }
        if wayland.contains(PollFlags::IN) {
            read.read()?;
        } else {
            drop(read);
        }
        if !command.is_empty() {
            match wake.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

impl App {
    pub(crate) fn start(
        path: &Path,
        size: Size,
        pattern: Pattern,
        events: mpsc::Sender<Event>,
    ) -> Result<Self> {
        let (commands, incoming) = mpsc::sync_channel(64);
        let (wake, receiver) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let socket = UnixStream::connect(path)?;
        let client = socket.try_clone()?;
        let thread = thread::Builder::new()
            .name("bench-wayland-app".into())
            .spawn(move || {
                if let Err(error) = run(client, size, pattern, events.clone(), &incoming, receiver)
                {
                    let _ = events.send(Event::Error(format!("Wayland workload: {error}")));
                }
            })?;
        Ok(Self {
            commands,
            wake,
            socket,
            thread: Some(thread),
        })
    }

    pub(crate) fn send(&self, command: Command) -> Result<()> {
        self.commands.send(command)?;
        (&self.wake).write_all(&[1])?;
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.commands.try_send(Command::Stop);
        let _ = (&self.wake).write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_millis(250);
            while !thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if !thread.is_finished() {
                let _ = self.socket.shutdown(Shutdown::Both);
            }
            let _ = thread.join();
        }
    }
}
