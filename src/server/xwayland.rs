//! X display reservation and xwayland-satellite startup.

use std::{
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    os::{fd::AsRawFd as _, unix::net::UnixListener},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
};

use nutype::nutype;
use rustix::{
    io::{Errno, FdFlags, fcntl_setfd},
    net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType},
};

use crate::Error;

/// An X11 display number reserved for xwayland-satellite.
#[nutype(
    const_fn,
    derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display)
)]
pub struct XDisplayNumber(u32);

/// The display numbers the server tries, in order.
const DISPLAY_SLOTS: std::ops::RangeInclusive<u32> = 0..=32;
const SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";

/// One xwayland-satellite child and the X display it reserved.
///
/// Dropping this stops the child and releases the display.
#[derive(Debug)]
pub struct Server {
    display: String,
    child: Option<Child>,
    _listeners: Vec<UnixListener>,
    _lock: DisplayLock,
}

impl Server {
    pub fn start(wayland_display: &str) -> Result<Self, Error> {
        let (lock, listeners) = reserve_display()?;
        let x_display = format!(":{}", lock.number);

        // The child takes the sockets as `-listenfd`, so it must inherit them.
        for listener in &listeners {
            fcntl_setfd(listener, FdFlags::empty()).map_err(std::io::Error::from)?;
        }
        let mut command = Command::new("xwayland-satellite");
        command
            .arg(&x_display)
            .env("WAYLAND_DISPLAY", wayland_display)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for listener in &listeners {
            command
                .arg("-listenfd")
                .arg(listener.as_raw_fd().to_string());
        }
        crate::server::process::spawn_unblocked(&mut command);
        let child = command.spawn();
        let mut reset_error = None;
        // The flag goes back on, so no later child inherits the sockets.
        for listener in &listeners {
            if let Err(error) = fcntl_setfd(listener, FdFlags::CLOEXEC) {
                reset_error.get_or_insert_with(|| std::io::Error::from(error));
            }
        }
        let mut child = child?;
        if let Some(error) = reset_error {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }

        tracing::info!(display = %x_display, pid = child.id(), "xwayland-satellite started");
        Ok(Self {
            display: x_display,
            child: Some(child),
            _listeners: listeners,
            _lock: lock,
        })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let Some(child) = &mut self.child else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
struct DisplayLock {
    number: XDisplayNumber,
}

impl DisplayLock {
    /// Create the lock file for one display, or take over a stale one.
    fn acquire(number: XDisplayNumber) -> std::io::Result<Self> {
        let path = lock_path(number);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => write_lock(file, &path, number),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                remove_stale_lock(&path)?;
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                write_lock(file, &path, number)
            }
            Err(error) => Err(error),
        }
    }
}

fn write_lock(mut file: File, path: &Path, number: XDisplayNumber) -> std::io::Result<DisplayLock> {
    if let Err(error) = writeln!(file, "{:>10}", std::process::id()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(DisplayLock { number })
}

impl Drop for DisplayLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(socket_path(self.number));
        let _ = std::fs::remove_file(lock_path(self.number));
    }
}

fn reserve_display() -> std::io::Result<(DisplayLock, Vec<UnixListener>)> {
    std::fs::create_dir_all(SOCKET_DIRECTORY)?;
    let mut last_error = None;
    for raw in DISPLAY_SLOTS {
        let number = XDisplayNumber::new(raw);
        let lock = match DisplayLock::acquire(number) {
            Ok(lock) => lock,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        match open_listeners(number) {
            Ok(listeners) => return Ok((lock, listeners)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrInUse, "all X11 displays are in use")
    }))
}

/// Open the filesystem and abstract X sockets of one display.
///
/// X clients use either address. A socket file left by a dead display is
/// removed first.
fn open_listeners(number: XDisplayNumber) -> std::io::Result<Vec<UnixListener>> {
    let path = socket_path(number);
    let _ = std::fs::remove_file(&path);
    let filesystem = SocketAddrUnix::new(path.as_os_str().as_encoded_bytes())?;
    let abstract_name = path.as_os_str().as_encoded_bytes();
    let abstract_socket = SocketAddrUnix::new_abstract_name(abstract_name)?;
    Ok(vec![
        open_listener(&filesystem)?,
        open_listener(&abstract_socket)?,
    ])
}

fn open_listener(address: &SocketAddrUnix) -> std::io::Result<UnixListener> {
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::net::bind(&fd, address)?;
    rustix::net::listen(&fd, 16)?;
    Ok(UnixListener::from(fd))
}

/// Remove the lock file if its owner process is gone.
fn remove_stale_lock(path: &Path) -> std::io::Result<()> {
    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    let raw = contents
        .trim()
        .parse::<i32>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let pid = rustix::process::Pid::from_raw(raw).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid X11 lock owner")
    })?;
    match rustix::process::test_kill_process(pid) {
        Err(Errno::SRCH) => std::fs::remove_file(path),
        Ok(()) | Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "X11 display is already owned",
        )),
    }
}

fn socket_path(number: XDisplayNumber) -> std::path::PathBuf {
    Path::new(SOCKET_DIRECTORY).join(format!("X{number}"))
}

fn lock_path(number: XDisplayNumber) -> std::path::PathBuf {
    Path::new("/tmp").join(format!(".X{number}-lock"))
}
