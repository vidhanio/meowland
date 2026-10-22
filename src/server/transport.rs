//! Socket ownership and I/O threads. The server event loop owns all state;
//! transport threads only decode/encode messages and report arrivals.

use std::{
    collections::HashMap,
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

use calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken, channel,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};

use super::{Incoming, IncomingSender, Paths, Reply};
use crate::{
    Error, Result,
    compositor::PaneId,
    pixels::FrameSize,
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
    path: Option<SocketPath>,
    wake: (UnixStream, UnixStream),
}

/// Adopt both systemd listeners by pathname, regardless of descriptor order,
/// or bind standalone sockets. Inherited paths remain owned by the manager.
/// Call before spawning threads because `ListenFd` consumes activation
/// variables.
pub(super) fn listeners(paths: &Paths) -> Result<(BoundSocket, BoundSocket)> {
    let mut inherited = listenfd::ListenFd::from_env();
    if inherited.len() == 0 {
        return Ok((bind(&paths.control)?, bind(&paths.pane)?));
    }
    if inherited.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket activation requires exactly two listeners",
        )
        .into());
    }
    let mut control = None;
    let mut pane = None;
    for index in 0..2 {
        let listener = inherited.take_unix_listener(index)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing activation listener")
        })?;
        let address = listener.local_addr()?;
        let slot = match address.as_pathname() {
            Some(path) if path == paths.control => &mut control,
            Some(path) if path == paths.pane => &mut pane,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unexpected activation socket path",
                )
                .into());
            }
        };
        if slot.is_some()
            || !rustix::net::sockopt::socket_acceptconn(&listener).map_err(io::Error::from)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate or non-listening activation socket",
            )
            .into());
        }
        listener.set_nonblocking(true)?;
        *slot = Some(BoundSocket {
            listener,
            path: None,
            wake: UnixStream::pair()?,
        });
    }
    match (control, pane) {
        (Some(control), Some(pane)) => Ok((control, pane)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "missing activation socket").into()),
    }
}

fn bind(path: &Path) -> Result<BoundSocket> {
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
    let owned_path = SocketPath {
        path: path.to_path_buf(),
        identity,
    };
    let socket = BoundSocket {
        listener,
        path: Some(owned_path),
        wake: UnixStream::pair()?,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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

impl AcceptLoop {
    pub(super) fn stop_accepting(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        self.path.take();
    }
}

impl Drop for AcceptLoop {
    fn drop(&mut self) {
        self.stop_accepting();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
struct ConnectionThread {
    socket: UnixStream,
    /// Cancellation authority, relinquished before handing a pane to the
    /// server.
    owned: Arc<AtomicBool>,
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

pub(super) fn accept_control(socket: BoundSocket, incoming: IncomingSender) -> AcceptLoop {
    accept_connections(socket, incoming, "control", |mut socket, incoming, _| {
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

pub(super) fn accept_panes(socket: BoundSocket, incoming: IncomingSender) -> AcceptLoop {
    accept_connections(socket, incoming, "pane", |mut socket, incoming, owned| {
        let _ = socket.set_read_timeout(Some(HELLO_TIMEOUT));
        let _ = socket.set_write_timeout(Some(HANDSHAKE_WRITE_TIMEOUT));
        match protocol::recv::<PaneToServer>(&mut socket) {
            Ok(PaneToServer::Hello(hello)) => {
                if FrameSize::new(hello.width, hello.height).is_none() {
                    let _ = protocol::send(
                        &mut socket,
                        &ServerToPane::Reject("invalid pane dimensions".into()),
                    );
                    return;
                }
                let _ = socket.set_read_timeout(None);
                if owned.swap(false, Ordering::AcqRel) {
                    let _ = incoming.send(Incoming::Pane(hello, socket));
                }
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
    incoming: IncomingSender,
    label: &'static str,
    handle: F,
) -> AcceptLoop
where
    F: Fn(UnixStream, IncomingSender, &AtomicBool) + Copy + Send + 'static,
{
    let BoundSocket {
        listener,
        path,
        wake: (cancel, wake),
    } = socket;
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let thread = thread::spawn(move || {
        if let Err(error) = run_acceptor(listener, cancel, incoming, stop, handle) {
            tracing::error!(%error, label, "accept readiness stopped");
        }
    });
    AcceptLoop {
        path,
        stopping,
        wake,
        thread: Some(thread),
    }
}

struct Acceptor {
    incoming: IncomingSender,
    completed: channel::Sender<u64>,
    workers: HashMap<u64, ConnectionThread>,
    next: u64,
    running: bool,
    handle: LoopHandle<'static, Self>,
    listener: Option<RegistrationToken>,
}

impl Acceptor {
    fn spawn<F>(&mut self, socket: UnixStream, handle: F) -> io::Result<()>
    where
        F: Fn(UnixStream, IncomingSender, &AtomicBool) + Send + 'static,
    {
        let cancellation = socket.try_clone()?;
        let incoming = self.incoming.clone();
        let completed = self.completed.clone();
        let id = self.next;
        self.next += 1;
        let owned = Arc::new(AtomicBool::new(true));
        let ownership = Arc::clone(&owned);
        let thread = thread::Builder::new().spawn(move || {
            handle(socket, incoming, &ownership);
            ownership.store(false, Ordering::Release);
            let _ = completed.send(id);
        })?;
        self.workers.insert(
            id,
            ConnectionThread {
                socket: cancellation,
                owned,
                thread,
            },
        );
        Ok(())
    }

    fn retry(&self) -> io::Result<()> {
        let handle = self.handle.clone();
        let listener = self.listener;
        self.handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(50)),
                move |_, (), acceptor| {
                    if let Some(listener) = listener
                        && let Err(error) = handle.enable(&listener)
                    {
                        tracing::error!(%error, "cannot resume listener");
                        acceptor.running = false;
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(())
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        for worker in self.workers.values() {
            if worker.owned.swap(false, Ordering::AcqRel) {
                let _ = worker.socket.shutdown(std::net::Shutdown::Both);
            }
        }
        for (_, worker) in self.workers.drain() {
            let _ = worker.thread.join();
        }
    }
}

fn run_acceptor<F>(
    listener: UnixListener,
    cancel: UnixStream,
    incoming: IncomingSender,
    stop: Arc<AtomicBool>,
    handle: F,
) -> io::Result<()>
where
    F: Fn(UnixStream, IncomingSender, &AtomicBool) + Copy + Send + 'static,
{
    let mut event_loop = EventLoop::<Acceptor>::try_new().map_err(io::Error::other)?;
    let loop_handle = event_loop.handle();
    let (completed, completions) = channel::channel();
    loop_handle
        .insert_source(completions, |event, (), acceptor| {
            if let channel::Event::Msg(id) = event
                && let Some(worker) = acceptor.workers.remove(&id)
            {
                let _ = worker.thread.join();
            }
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    loop_handle
        .insert_source(
            Generic::new(cancel, Interest::READ, Mode::Level),
            |_, _, acceptor| {
                acceptor.running = false;
                Ok(PostAction::Remove)
            },
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    let listener = loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, acceptor| {
                if stop.load(Ordering::Relaxed) {
                    acceptor.running = false;
                    return Ok(PostAction::Remove);
                }
                match listener.accept() {
                    Ok((socket, _)) => {
                        if let Err(error) = acceptor.spawn(socket, handle) {
                            tracing::warn!(%error, "cannot own accepted connection");
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if transient_accept(&error) => {
                        tracing::warn!(%error, "accept failed; retrying");
                        acceptor.retry()?;
                        return Ok(PostAction::Disable);
                    }
                    Err(error) => return Err(error),
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut acceptor = Acceptor {
        incoming,
        completed,
        workers: HashMap::new(),
        next: 1,
        running: true,
        handle: loop_handle,
        listener: Some(listener),
    };
    while acceptor.running {
        event_loop
            .dispatch(None, &mut acceptor)
            .map_err(io::Error::other)?;
    }
    Ok(())
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
        id: PaneId,
        socket: UnixStream,
        writer: UnixStream,
        incoming: IncomingSender,
    ) -> io::Result<Self> {
        let cancellation = socket.try_clone()?;
        let (sender, receiver) = mpsc::channel();
        let completed = incoming.clone();
        let writer = thread::spawn(move || {
            pane_writer(writer, receiver);
            let _ = completed.send(Incoming::PaneWriterDone(id));
        });
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
}

impl Drop for Pane {
    fn drop(&mut self) {
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
    let _ = socket.shutdown(std::net::Shutdown::Both);
}
