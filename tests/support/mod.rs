//! Process-level fixtures: a directly spawned server and a raw Wayland client.
#![expect(
    dead_code,
    reason = "each test binary uses a different subset of the shared fixture"
)]

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::{
        fd::AsFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt as _},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

pub mod dmabuf;
pub mod fake;

use meowland::protocol::{self, Hello, PaneToServer, ServerToPane, Show};

pub const BINARY: &str = env!("CARGO_BIN_EXE_meowland");

pub fn temp_dir(prefix: &str) -> PathBuf {
    // The counter separates same-nanosecond calls; the timestamp separates
    // runs.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{unique}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

/// Server in a private runtime directory, spawned without a user manager.
pub struct Server {
    pub runtime: PathBuf,
    child: Child,
    stopped: bool,
}

impl Server {
    /// Disable Xwayland to avoid depending on `xwayland-satellite`.
    pub fn start() -> Self {
        Self::start_with_env(&[("MEOWLAND_XWAYLAND", "off")])
    }

    pub fn start_with_env(env: &[(&str, &str)]) -> Self {
        let runtime = temp_dir("meowland-test");
        let stderr = fs::File::create(runtime.join("server.stderr")).unwrap();
        let mut command = Command::new(BINARY);
        command
            .arg("server")
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        for (key, value) in env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn meowland server");
        let server = Self {
            runtime,
            child,
            stopped: false,
        };
        assert!(
            server.wait_for_control(Duration::from_secs(5)),
            "server did not accept control requests"
        );
        server
    }

    fn wait_for_control(&self, timeout: Duration) -> bool {
        wait_for(timeout, || {
            let Ok(mut socket) = UnixStream::connect(self.runtime.join("meowland-control.sock"))
            else {
                return false;
            };
            let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
            protocol::send(&mut socket, &protocol::ControlRequest::Ping).is_ok()
                && matches!(
                    protocol::recv::<protocol::ControlResponse>(&mut socket),
                    Ok(protocol::ControlResponse::Ok)
                )
        })
    }

    pub fn control(&self, request: &protocol::ControlRequest) -> protocol::ControlResponse {
        let mut socket = UnixStream::connect(self.runtime.join("meowland-control.sock"))
            .expect("control socket");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        protocol::send(&mut socket, request).unwrap();
        protocol::recv(&mut socket).unwrap()
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        Command::new(BINARY)
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .output()
            .expect("run meowland")
    }

    pub fn list(&self) -> Vec<protocol::WindowInfo> {
        match self.control(&protocol::ControlRequest::List) {
            protocol::ControlResponse::Windows(windows) => windows,
            protocol::ControlResponse::Error(error) => panic!("list failed: {error}"),
            protocol::ControlResponse::Ok => panic!("unexpected list reply"),
        }
    }

    pub fn wait_for_window(&self, timeout: Duration) -> bool {
        wait_for(timeout, || !self.list().is_empty())
    }

    pub fn log(&self) -> String {
        fs::read_to_string(self.runtime.join("server.stderr")).unwrap_or_default()
    }

    pub fn wayland_socket(&self) -> PathBuf {
        assert!(
            wait_for(Duration::from_secs(5), || self
                .find_wayland_socket()
                .is_some()),
            "server did not create a Wayland socket"
        );
        self.find_wayland_socket().unwrap()
    }

    /// Select the socket, not its similarly named `.lock` file.
    fn find_wayland_socket(&self) -> Option<PathBuf> {
        fs::read_dir(&self.runtime)
            .ok()?
            .flatten()
            .find(|entry| {
                let name = entry.file_name();
                let is_socket = entry.file_type().is_ok_and(|kind| kind.is_socket());
                is_socket && name.to_string_lossy().starts_with("wayland-meowland-")
            })
            .map(|entry| entry.path())
    }

    /// Assert the server exits, including its log on failure.
    pub fn stop(&mut self) {
        assert!(
            self.shutdown(),
            "server did not exit;\nlog:\n{}",
            self.log()
        );
    }

    /// Avoid panicking in `Drop` while a failed test is unwinding.
    fn shutdown(&mut self) -> bool {
        if self.stopped {
            return true;
        }
        self.stopped = true;
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        let stopped = wait_for(Duration::from_secs(5), || {
            matches!(self.child.try_wait(), Ok(Some(_)))
                && !self.runtime.join("meowland-control.sock").exists()
        });
        if !stopped {
            eprintln!("server did not exit;\nlog:\n{}", self.log());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        stopped
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("server stderr during test failure:\n{}", self.log());
        }
        let _ = self.shutdown();
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

/// Speak the first X11 setup message and report whether the server accepts
/// the connection (reply byte 1) or rejects it (0).
pub fn x11_connection_succeeds(socket: &std::path::Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut setup = vec![b'l', 0];
    setup.extend_from_slice(&11u16.to_le_bytes());
    setup.extend_from_slice(&0u16.to_le_bytes());
    setup.extend_from_slice(&0u16.to_le_bytes());
    setup.extend_from_slice(&0u16.to_le_bytes());
    setup.extend_from_slice(&[0, 0]);
    if stream.write_all(&setup).is_err() {
        return false;
    }
    let mut reply = [0u8; 1];
    matches!(stream.read_exact(&mut reply), Ok(()) if reply[0] == 1)
}

pub struct Pty {
    pub master: fs::File,
    pub slave: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub cell: (u16, u16),
}

impl Pty {
    pub fn open(rows: u16, cols: u16, cell: (u16, u16)) -> Self {
        let master =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
                .expect("open pty");
        rustix::pty::grantpt(&master).unwrap();
        rustix::pty::unlockpt(&master).unwrap();
        let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
        rustix::termios::tcsetwinsize(
            &master,
            rustix::termios::Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: cols * cell.0,
                ws_ypixel: rows * cell.1,
            },
        )
        .unwrap();
        Self {
            master: fs::File::from(master),
            slave: PathBuf::from(name.to_string_lossy().into_owned()),
            rows,
            cols,
            cell,
        }
    }

    pub fn pixels(&self) -> (u32, u32) {
        (
            u32::from(self.cols) * u32::from(self.cell.0),
            u32::from(self.rows) * u32::from(self.cell.1),
        )
    }

    /// Create a separate session so `/dev/tty` cannot access the test runner's
    /// terminal and crossterm uses this pty on stdin/stdout.
    #[expect(
        unsafe_code,
        reason = "`pre_exec` is unsafe because its closure runs between fork and exec"
    )]
    pub fn spawn(&self, command: &mut Command) -> PtyChild {
        let slave = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.slave)
            .unwrap();
        command
            .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(slave));
        // SAFETY: `pre_exec` runs between fork and exec; `setsid` makes no
        // allocations, and failure to detach is deliberately ignored.
        unsafe {
            command.pre_exec(|| {
                let _ = rustix::process::setsid();
                Ok(())
            });
        }
        PtyChild(command.spawn().expect("spawn on pty"))
    }

    pub fn read_now(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut fds = [rustix::event::PollFd::new(
                &self.master,
                rustix::event::PollFlags::IN,
            )];
            let zero = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if rustix::event::poll(&mut fds, Some(&zero)).unwrap() == 0 {
                break;
            }
            match self.master.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                // EIO is how a pty reports that the child closed its side.
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("pty read failed: {error}"),
            }
        }
        bytes
    }
}

/// Pane process killed and reaped on drop, including after assertion failures.
#[derive(Debug)]
pub struct PtyChild(Child);

impl PtyChild {
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    pub fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_raw(i32::try_from(self.0.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn program_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn message(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
    let size = 8 + args.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&object.to_ne_bytes());
    out.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    out.extend_from_slice(args);
    out
}

pub fn i32s(values: &[i32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

/// A 24.8 fixed point number, the encoding the viewporter uses.
pub fn fixed(value: f64) -> i32 {
    (value * 256.0).round() as i32
}

pub fn u32s(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

pub fn string_arg(value: &str) -> Vec<u8> {
    let mut out = ((value.len() as u32) + 1).to_ne_bytes().to_vec();
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out
}

#[derive(Debug)]
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

impl Message {
    pub fn u32_at(&self, index: usize) -> u32 {
        u32::from_ne_bytes(self.body[index * 4..index * 4 + 4].try_into().unwrap())
    }
}

/// A raw Wayland client connection.
pub struct Client {
    stream: UnixStream,
    buffer: Vec<u8>,
    next_id: u32,
    pub globals: HashMap<String, u32>,
    pub compositor: u32,
    pub shm: u32,
    pub xdg: u32,
}

impl Client {
    pub fn connect(server: &Server) -> Self {
        let path = server.wayland_socket();
        let stream = UnixStream::connect(&path).unwrap_or_else(|error| {
            panic!(
                "connecting to {}: {error};\nlog:\n{}",
                path.display(),
                server.log()
            )
        });
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut client = Self {
            stream,
            buffer: Vec::new(),
            next_id: 3,
            globals: HashMap::new(),
            compositor: 0,
            shm: 0,
            xdg: 0,
        };
        // Request the registry, then wait for sync to finish its announcements.
        client.write(&message(1, 1, &u32s(&[2])));
        client.write(&message(1, 0, &u32s(&[3])));
        loop {
            let message = client.read();
            if message.object == 3 {
                break;
            }
            if message.object == 2 && message.opcode == 0 {
                let name = message.u32_at(0);
                let len = message.u32_at(1) as usize;
                let end = message.body[8..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(len);
                let interface = String::from_utf8_lossy(&message.body[8..8 + end]).into_owned();
                client.globals.insert(interface, name);
            }
        }
        client.compositor = client.bind("wl_compositor", 4);
        client.shm = client.bind("wl_shm", 1);
        client.xdg = client.bind("xdg_wm_base", 1);
        client
    }

    pub fn has_global(&self, interface: &str) -> bool {
        self.globals.contains_key(interface)
    }

    pub const fn alloc(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    pub fn request(&mut self, object: u32, opcode: u16, args: &[u8]) {
        self.write(&message(object, opcode, args));
    }

    pub fn send_fd(&self, object: u32, opcode: u16, args: &[u8], fd: &impl AsFd) {
        let request = message(object, opcode, args);
        let mut control = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut control);
        let fds = [fd.as_fd()];
        ancillary.push(rustix::net::SendAncillaryMessage::ScmRights(&fds));
        let iov = [std::io::IoSlice::new(&request)];
        rustix::net::sendmsg(
            &self.stream,
            &iov,
            &mut ancillary,
            rustix::net::SendFlags::empty(),
        )
        .unwrap();
    }

    pub fn bind(&mut self, interface: &str, version: u32) -> u32 {
        let id = self.alloc();
        let name = *self
            .globals
            .get(interface)
            .unwrap_or_else(|| panic!("the compositor did not advertise {interface}"));
        let mut args = u32s(&[name]);
        args.extend_from_slice(&string_arg(interface));
        args.extend_from_slice(&u32s(&[version, id]));
        self.request(2, 0, &args);
        id
    }

    fn next_message(&mut self) -> Option<Message> {
        if self.buffer.len() < 8 {
            return None;
        }
        let word = u32::from_ne_bytes(self.buffer[4..8].try_into().unwrap());
        let size = (word >> 16) as usize;
        assert!((8..=64 * 1024 * 1024).contains(&size), "bad size {size}");
        if self.buffer.len() < size {
            return None;
        }
        let message = Message {
            object: u32::from_ne_bytes(self.buffer[..4].try_into().unwrap()),
            opcode: word as u16,
            body: self.buffer[8..size].to_vec(),
        };
        self.buffer.drain(..size);
        Some(message)
    }

    fn fill(&mut self) {
        let mut chunk = vec![0u8; 64 * 1024];
        let read = self.stream.read(&mut chunk).unwrap();
        assert!(read > 0, "the server closed the connection");
        self.buffer.extend_from_slice(&chunk[..read]);
    }

    pub fn read(&mut self) -> Message {
        loop {
            if let Some(message) = self.next_message() {
                return message;
            }
            self.fill();
        }
    }

    /// Everything the server has already sent, without blocking for more.
    pub fn drain(&mut self) -> Vec<Message> {
        self.stream.set_nonblocking(true).unwrap();
        let mut messages = Vec::new();
        loop {
            while let Some(message) = self.next_message() {
                messages.push(message);
            }
            let mut chunk = vec![0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("read failed: {error}"),
            }
        }
        self.stream.set_nonblocking(false).unwrap();
        messages
    }

    pub fn read_until(&mut self, mut predicate: impl FnMut(&Message) -> bool) -> Message {
        loop {
            let message = self.read();
            if predicate(&message) {
                return message;
            }
        }
    }

    /// Round-trip the connection: everything the server had already sent
    /// arrives before the sync callback does.
    pub fn sync(&mut self) -> Vec<Message> {
        let callback = self.alloc();
        self.request(1, 0, &u32s(&[callback]));
        let mut seen = Vec::new();
        loop {
            let message = self.read();
            if message.object == callback && message.opcode == 0 {
                return seen;
            }
            seen.push(message);
        }
    }

    pub fn create_surface(&mut self) -> u32 {
        let id = self.alloc();
        self.request(self.compositor, 0, &u32s(&[id]));
        id
    }

    /// Upload a `wl_shm` buffer via `SCM_RIGHTS`; unlink its backing file.
    pub fn shm_buffer(&mut self, width: u32, height: u32, stride: u32, pixels: &[u8]) -> u32 {
        self.shm_buffer_with_file(width, height, stride, pixels).0
    }

    /// Return the writable backing file for modifying pixels after upload.
    pub fn shm_buffer_with_file(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) -> (u32, fs::File) {
        let pool_id = self.alloc();
        let buffer_id = self.alloc();
        let staging = temp_dir("meowland-shm");
        let path = staging.join("buffer");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(u64::from(stride) * u64::from(height)).unwrap();
        (&file).write_all(pixels).unwrap();
        self.send_fd(self.shm, 0, &u32s(&[pool_id, stride * height]), &file);
        // The sent pool fd keeps the inode alive after unlinking.
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&staging).unwrap();
        // Argb8888 pixels are B, G, R, alpha; the compositor ignores alpha.
        self.request(pool_id, 0, &u32s(&[buffer_id, 0, width, height, stride, 0]));
        self.request(pool_id, 1, &[]);
        (buffer_id, file)
    }

    pub fn create_toplevel(&mut self, title: &str, app_id: &str) -> Window {
        let surface = self.create_surface();
        let xdg_surface = self.alloc();
        self.request(self.xdg, 2, &u32s(&[xdg_surface, surface]));
        let toplevel = self.alloc();
        self.request(xdg_surface, 1, &u32s(&[toplevel]));
        self.request(toplevel, 2, &string_arg(title));
        self.request(toplevel, 3, &string_arg(app_id));
        self.request(surface, 6, &[]);
        let configure =
            self.read_until(|message| message.object == xdg_surface && message.opcode == 0);
        self.request(xdg_surface, 4, &u32s(&[configure.u32_at(0)]));
        Window {
            surface,
            xdg_surface,
            xdg_toplevel: toplevel,
        }
    }

    pub fn attach(&mut self, toplevel: &Window, buffer: u32, width: u32, height: u32) {
        self.attach_surface(toplevel.surface, buffer, width, height);
    }

    pub fn attach_surface(&mut self, surface: u32, buffer: u32, width: u32, height: u32) {
        self.request(surface, 1, &u32s(&[buffer, 0, 0]));
        self.request(surface, 2, &u32s(&[0, 0, width, height]));
        self.request(surface, 6, &[]);
    }

    pub fn commit(&mut self, surface: u32) {
        self.request(surface, 6, &[]);
    }

    pub fn set_title(&mut self, toplevel: u32, title: &str) {
        self.request(toplevel, 2, &string_arg(title));
    }

    pub fn set_window_geometry(&mut self, xdg_surface: u32, x: i32, y: i32, w: i32, h: i32) {
        self.request(xdg_surface, 3, &i32s(&[x, y, w, h]));
    }

    pub fn create_subsurface(&mut self, surface: u32, parent: u32) -> u32 {
        let subcompositor = self.bind("wl_subcompositor", 1);
        let id = self.alloc();
        self.request(subcompositor, 1, &u32s(&[id, surface, parent]));
        id
    }

    pub fn subsurface_position(&mut self, subsurface: u32, x: i32, y: i32) {
        self.request(subsurface, 1, &i32s(&[x, y]));
    }

    pub fn subsurface_desync(&mut self, subsurface: u32) {
        self.request(subsurface, 5, &[]);
    }

    /// Anchor the popup within the parent's window geometry.
    pub fn create_popup(
        &mut self,
        parent_xdg_surface: u32,
        size: (i32, i32),
        anchor_rect: (i32, i32, i32, i32),
    ) -> Popup {
        let surface = self.create_surface();
        let xdg_surface = self.alloc();
        self.request(self.xdg, 2, &u32s(&[xdg_surface, surface]));
        let positioner = self.alloc();
        self.request(self.xdg, 1, &u32s(&[positioner]));
        self.request(positioner, 1, &i32s(&<[i32; 2]>::from(size)));
        self.request(positioner, 2, &i32s(&<[i32; 4]>::from(anchor_rect)));
        self.request(positioner, 3, &u32s(&[5]));
        self.request(positioner, 4, &u32s(&[8]));
        self.request(positioner, 5, &u32s(&[0]));
        let popup = self.alloc();
        self.request(
            xdg_surface,
            2,
            &u32s(&[popup, parent_xdg_surface, positioner]),
        );
        self.request(positioner, 0, &[]);
        Popup {
            surface,
            xdg_surface,
            handle: popup,
        }
    }

    pub fn create_viewport(&mut self, surface: u32) -> u32 {
        let viewporter = self.bind("wp_viewporter", 1);
        let id = self.alloc();
        self.request(viewporter, 1, &u32s(&[id, surface]));
        id
    }

    pub fn viewport_source(&mut self, viewport: u32, x: f64, y: f64, w: f64, h: f64) {
        self.request(
            viewport,
            1,
            &i32s(&[fixed(x), fixed(y), fixed(w), fixed(h)]),
        );
    }

    pub fn viewport_destination(&mut self, viewport: u32, w: i32, h: i32) {
        self.request(viewport, 2, &i32s(&[w, h]));
    }

    pub fn seat(&mut self) -> u32 {
        self.bind("wl_seat", 1)
    }

    pub fn get_keyboard(&mut self, seat: u32) -> u32 {
        let id = self.alloc();
        self.request(seat, 1, &u32s(&[id]));
        id
    }

    pub fn get_pointer(&mut self, seat: u32) -> u32 {
        let id = self.alloc();
        self.request(seat, 0, &u32s(&[id]));
        id
    }

    /// Frame callbacks are double-buffered, so commit the request.
    pub fn frame_callback(&mut self, surface: u32) -> u32 {
        let id = self.alloc();
        self.request(surface, 3, &u32s(&[id]));
        self.request(surface, 6, &[]);
        id
    }

    pub fn destroy_toplevel(&mut self, window: &Window) {
        self.request(window.xdg_toplevel, 0, &[]);
        self.request(window.xdg_surface, 0, &[]);
        self.request(window.surface, 0, &[]);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub surface: u32,
    pub xdg_surface: u32,
    pub xdg_toplevel: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Popup {
    pub surface: u32,
    pub xdg_surface: u32,
    pub handle: u32,
}

impl Popup {
    /// Read the popup's configure, acknowledge it and map the popup with
    /// `pixels`.  Returns the position the compositor asked for, in window
    /// geometry coordinates.
    pub fn map(&self, client: &mut Client, width: u32, height: u32, pixels: &[u8]) -> (i32, i32) {
        let configure = client.read_until(|message| message.object == self.handle);
        assert_eq!(configure.opcode, 0, "xdg_popup.configure");
        let position = (configure.u32_at(0) as i32, configure.u32_at(1) as i32);
        let serial = client
            .read_until(|message| message.object == self.xdg_surface)
            .u32_at(0);
        client.request(self.xdg_surface, 4, &u32s(&[serial]));
        let buffer = client.shm_buffer(width, height, width * 4, pixels);
        client.attach_surface(self.surface, buffer, width, height);
        position
    }
}

/// The pane side of the protocol.
pub struct Pane {
    stream: UnixStream,
    /// The frame as received, band by band: the compositor sends only the rows
    /// that changed, so a test composes them the way a pane does.
    image: Vec<u8>,
    width: u32,
    height: u32,
    /// The first row and the row count of the last band.
    band: (u32, u32),
}

impl Pane {
    pub fn attach(server: &Server, hello: Hello) -> Self {
        let mut stream = UnixStream::connect(server.runtime.join("meowland-pane.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        protocol::send(&mut stream, &PaneToServer::Hello(hello)).unwrap();
        let mut pane = Self {
            stream,
            image: Vec::new(),
            width: 0,
            height: 0,
            band: (0, 0),
        };
        match pane.recv() {
            ServerToPane::HelloOk => pane,
            other => panic!("pane handshake failed: {other:?}"),
        }
    }

    pub fn recv(&mut self) -> ServerToPane {
        protocol::recv(&mut self.stream).unwrap()
    }

    /// The next frame, ignoring the title and pointer shape updates that can
    /// arrive before it.  Bands are composed into the frame they belong to, so
    /// the whole picture comes back whichever rows moved.
    pub fn frame(&mut self) -> (u32, u32, Vec<u8>) {
        loop {
            match self.recv() {
                ServerToPane::Frame {
                    width,
                    height,
                    y,
                    rgb,
                } => {
                    self.apply(width, height, y, &rgb);
                    return (self.width, self.height, self.image.clone());
                }
                ServerToPane::Title(_) | ServerToPane::Cursor(_) => {}
                other => panic!("expected a frame, got {other:?}"),
            }
        }
    }

    /// The rows of the frame that arrived last: the first row and how many.
    pub const fn band(&self) -> (u32, u32) {
        self.band
    }

    fn apply(&mut self, width: u32, height: u32, y: u32, rgb: &[u8]) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.image.clear();
            self.image.resize(width as usize * height as usize * 3, 0);
        }
        let stride = width as usize * 3;
        let start = y as usize * stride;
        self.image[start..start + rgb.len()].copy_from_slice(rgb);
        self.band = (y, u32::try_from(rgb.len() / stride).unwrap_or(0));
    }

    pub fn send(&mut self, message: &PaneToServer) {
        protocol::send(&mut self.stream, message).unwrap();
    }
}

pub const fn hello(width: u32, height: u32, show: Show) -> Hello {
    Hello {
        version: protocol::VERSION,
        width,
        height,
        show,
    }
}
