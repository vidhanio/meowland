//! Socket ownership and I/O threads. The server event loop owns all state;
//! transport threads only decode/encode messages and report arrivals.

use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use super::{Incoming, Reply};
use crate::{
    Error, Result,
    protocol::{self, ControlRequest, PaneToServer, ServerToPane},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn at(path: &Path) -> io::Result<Option<Self>> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(metadata.file_type().is_socket().then_some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
}

/// Only the socket created by this bind belongs to the server.
#[derive(Debug)]
struct SocketPath {
    path: PathBuf,
    identity: SocketIdentity,
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if SocketIdentity::at(&self.path).ok() == Some(Some(self.identity)) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
pub(super) struct BoundSocket {
    listener: UnixListener,
    path: SocketPath,
    wake: (UnixStream, UnixStream),
}

pub(super) fn bind(path: &Path) -> Result<BoundSocket> {
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let identity = match SocketIdentity::at(path) {
                Ok(Some(identity)) => identity,
                Ok(None) => return Err(error.into()),
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    return own_socket(path, UnixListener::bind(path)?);
                }
                Err(error) => return Err(error.into()),
            };
            match UnixStream::connect(path) {
                Ok(_) => return Err(Error::ServerAlreadyListening(path.to_path_buf())),
                Err(refused) if refused.kind() == io::ErrorKind::ConnectionRefused => {
                    // Do not unlink a path replaced while the listener was
                    // probed.
                    match SocketIdentity::at(path) {
                        Ok(Some(current)) if current == identity => match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        },
                        Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                        _ => return Err(error.into()),
                    }
                }
                // A concurrent owner may have removed its path. Retry the bind
                // without removing anything; a replacement still wins.
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            UnixListener::bind(path)?
        }
        Err(error) => return Err(error.into()),
    };
    own_socket(path, listener)
}

fn own_socket(path: &Path, listener: UnixListener) -> Result<BoundSocket> {
    let identity = SocketIdentity::at(path)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::AddrInUse, "bound socket path was replaced")
    })?;
    // Establish path ownership before allocating the cancellation channel so
    // a failed setup still removes only the socket we just bound.
    let path = SocketPath {
        path: path.to_path_buf(),
        identity,
    };
    let socket = BoundSocket {
        listener,
        path,
        wake: UnixStream::pair()?,
    };
    fs::set_permissions(&socket.path.path, fs::Permissions::from_mode(0o600))?;
    socket.listener.set_nonblocking(true)?;
    Ok(socket)
}

#[derive(Debug)]
pub(super) struct AcceptLoop {
    path: Option<SocketPath>,
    stopping: Arc<AtomicBool>,
    wake: UnixStream,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for AcceptLoop {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        // The read half wakes poll immediately, including a stop that arrived
        // just before the worker entered the syscall.
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        // Stop advertising the socket before waiting for its workers.
        self.path.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
struct ConnectionThread {
    socket: UnixStream,
    thread: thread::JoinHandle<()>,
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

fn transient_accept(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .iter()
        .any(|candidate| Some(candidate.raw_os_error()) == error.raw_os_error())
}

pub(super) fn accept_control(socket: BoundSocket, incoming: Sender<Incoming>) -> AcceptLoop {
    accept_connections(socket, incoming, "control", |mut socket, incoming| {
        let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
        if let Ok(request) = protocol::recv::<ControlRequest>(&mut socket) {
            let (reply_tx, reply_rx) = mpsc::channel();
            let waiting = Arc::new(AtomicBool::new(true));
            let reply = Reply {
                sender: reply_tx,
                waiting: Arc::clone(&waiting),
            };
            if incoming.send(Incoming::Control(request, reply)).is_ok()
                && let Ok(answer) = reply_rx.recv_timeout(Duration::from_secs(2))
            {
                let _ = protocol::send(&mut socket, &answer);
            }
            waiting.store(false, Ordering::Relaxed);
        }
    })
}

pub(super) fn accept_panes(socket: BoundSocket, incoming: Sender<Incoming>) -> AcceptLoop {
    accept_connections(socket, incoming, "pane", |mut socket, incoming| {
        let _ = socket.set_read_timeout(Some(HELLO_TIMEOUT));
        let _ = socket.set_write_timeout(Some(HANDSHAKE_WRITE_TIMEOUT));
        match protocol::recv::<PaneToServer>(&mut socket) {
            Ok(PaneToServer::Hello(hello)) => {
                let _ = socket.set_read_timeout(None);
                let _ = incoming.send(Incoming::Pane(hello, socket));
            }
            Ok(other) => {
                tracing::warn!(?other, "pane: first message was not a hello");
                let _ = protocol::send(
                    &mut socket,
                    &ServerToPane::Reject("the pane did not start with a hello".into()),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "pane: no hello");
                let _ = protocol::send(
                    &mut socket,
                    &ServerToPane::Reject(format!("the pane hello failed: {error}")),
                );
            }
        }
    })
}

fn accept_connections<F>(
    socket: BoundSocket,
    incoming: Sender<Incoming>,
    label: &'static str,
    handle: F,
) -> AcceptLoop
where
    F: Fn(UnixStream, Sender<Incoming>) + Copy + Send + 'static,
{
    let BoundSocket {
        listener,
        path,
        wake: (cancel, wake),
    } = socket;
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let thread = thread::spawn(move || {
        let mut workers: Vec<ConnectionThread> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let mut index = 0;
            while index < workers.len() {
                if workers[index].thread.is_finished() {
                    let worker = workers.swap_remove(index);
                    let _ = worker.thread.join();
                } else {
                    index += 1;
                }
            }
            let mut fds = [
                rustix::event::PollFd::new(&listener, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&cancel, rustix::event::PollFlags::IN),
            ];
            // The timeout only reaps finished handshake workers. Accepts and
            // shutdown wake on readiness, not on this housekeeping deadline.
            match rustix::event::poll(
                &mut fds,
                Some(&rustix::event::Timespec {
                    tv_sec: 1,
                    tv_nsec: 0,
                }),
            ) {
                Ok(0) | Err(rustix::io::Errno::INTR) => continue,
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(%error, label, "accept readiness stopped");
                    break;
                }
            }
            if stop.load(Ordering::Relaxed) || !fds[1].revents().is_empty() {
                break;
            }
            match listener.accept() {
                Ok((socket, _)) => {
                    let cancellation = match socket.try_clone() {
                        Ok(cancellation) => cancellation,
                        Err(error) => {
                            tracing::warn!(%error, label, "cannot own accepted connection");
                            continue;
                        }
                    };
                    let incoming = incoming.clone();
                    workers.push(ConnectionThread {
                        socket: cancellation,
                        thread: thread::spawn(move || handle(socket, incoming)),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if transient_accept(&error) => {
                    tracing::warn!(%error, label, "accept failed; retrying");
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    tracing::error!(%error, label, "accept stopped");
                    break;
                }
            }
        }
        drop(listener);
        for worker in &workers {
            if !worker.thread.is_finished() {
                let _ = worker.socket.shutdown(std::net::Shutdown::Both);
            }
        }
        for worker in workers {
            let _ = worker.thread.join();
        }
    });
    AcceptLoop {
        path: Some(path),
        stopping,
        wake,
        thread: Some(thread),
    }
}

/// Both halves of an established pane remain owned until they have stopped.
#[derive(Debug)]
pub(super) struct Pane {
    sender: Option<Sender<ServerToPane>>,
    socket: UnixStream,
    reader: Option<thread::JoinHandle<()>>,
    writer: Option<thread::JoinHandle<()>>,
}

impl Pane {
    pub(super) fn start(
        id: u64,
        socket: UnixStream,
        writer: UnixStream,
        incoming: Sender<Incoming>,
    ) -> io::Result<Self> {
        let cancellation = socket.try_clone()?;
        let (sender, receiver) = mpsc::channel();
        let writer = thread::spawn(move || pane_writer(writer, receiver));
        let reader = thread::spawn(move || {
            let mut socket = socket;
            while let Ok(message) = protocol::recv::<PaneToServer>(&mut socket) {
                if incoming.send(Incoming::PaneMessage(id, message)).is_err() {
                    break;
                }
            }
            let _ = incoming.send(Incoming::PaneGone(id));
        });
        Ok(Self {
            sender: Some(sender),
            socket: cancellation,
            reader: Some(reader),
            writer: Some(writer),
        })
    }

    pub(super) fn send(&self, message: ServerToPane) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(message);
        }
    }

    /// Drop the queue only after Release, never interrupting an in-flight
    /// frame.
    pub(super) fn release(&mut self, reason: String) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(ServerToPane::Release(reason));
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.writer
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // A library shutdown must also cancel a reader or blocked frame writer.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        self.sender.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// A stalled terminal must resume its frame rather than lose a partial packet.
fn pane_writer(mut socket: UnixStream, rx: Receiver<ServerToPane>) {
    // Cloned descriptors retain the handshake write timeout; frames need no
    // deadline.
    if let Err(error) = socket.set_write_timeout(None) {
        let _ = socket.shutdown(std::net::Shutdown::Both);
        tracing::warn!(%error, "pane: cannot clear handshake write timeout");
        return;
    }
    for message in rx {
        if let Err(error) = protocol::send(&mut socket, &message) {
            tracing::warn!(%error, "pane: write failed; connection closed");
            break;
        }
    }
    // Closing the peer-facing write half alone leaves the reader blocked while
    // its peer keeps the connection open after Release.
    let _ = socket.shutdown(std::net::Shutdown::Both);
}
