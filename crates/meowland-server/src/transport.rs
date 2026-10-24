//! Socket ownership, bounded connection tasks, and ordered pane output.

use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener as StdListener, UnixStream as StdStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use meowland_compositor::PaneId;
use meowland_core::{Error, Result, pixels::FrameSize};
use meowland_protocol::{
    self as protocol, ControlRequest, Hello, PaneToServer, ServerToPane,
    asynchronous::{Outgoing, Reader},
};
use tokio::{
    io::{AsyncReadExt, AsyncWrite},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinSet},
};

use super::{CONTROL_TIMEOUT, Incoming, IncomingSender, Reply};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PANE_QUEUE: usize = 8;

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

/// Only a socket created by this process belongs to the server.
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
pub struct BoundSocket {
    listener: UnixListener,
    _path: Option<SocketPath>,
}

pub struct Listeners {
    pub control: BoundSocket,
    pub pane: BoundSocket,
}

/// Adopt systemd listeners by pathname or bind standalone sockets.
/// Consume activation variables before starting the compositor thread.
pub fn listeners(paths: &protocol::Paths) -> Result<Listeners> {
    let mut inherited = listenfd::ListenFd::from_env();
    if inherited.len() == 0 {
        return Ok(Listeners {
            control: bind(&paths.control)?,
            pane: bind(&paths.pane)?,
        });
    }
    if inherited.len() != 2 {
        return Err(invalid("socket activation requires exactly two listeners").into());
    }
    let mut control = None;
    let mut pane = None;
    for index in 0..2 {
        let listener = inherited
            .take_unix_listener(index)?
            .ok_or_else(|| invalid("missing activation listener"))?;
        let address = listener.local_addr()?;
        let slot = match address.as_pathname() {
            Some(path) if path == paths.control => &mut control,
            Some(path) if path == paths.pane => &mut pane,
            _ => return Err(invalid("unexpected activation socket path").into()),
        };
        if slot.is_some()
            || !rustix::net::sockopt::socket_acceptconn(&listener).map_err(io::Error::from)?
        {
            return Err(invalid("duplicate or non-listening activation socket").into());
        }
        listener.set_nonblocking(true)?;
        *slot = Some(BoundSocket {
            listener: UnixListener::from_std(listener)?,
            _path: None,
        });
    }
    match (control, pane) {
        (Some(control), Some(pane)) => Ok(Listeners { control, pane }),
        _ => Err(invalid("missing activation socket").into()),
    }
}

fn bind(path: &Path) -> Result<BoundSocket> {
    let listener = match StdListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let identity = match SocketIdentity::at(path) {
                Ok(Some(identity)) => identity,
                Ok(None) => return Err(error.into()),
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    return own_socket(path, StdListener::bind(path)?);
                }
                Err(error) => return Err(error.into()),
            };
            match StdStream::connect(path) {
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
            StdListener::bind(path)?
        }
        Err(error) => return Err(error.into()),
    };
    own_socket(path, listener)
}

fn own_socket(path: &Path, listener: StdListener) -> Result<BoundSocket> {
    let identity =
        SocketIdentity::at(path)?.ok_or_else(|| invalid("bound socket path was replaced"))?;
    let owned_path = SocketPath {
        path: path.to_path_buf(),
        identity,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(BoundSocket {
        listener: UnixListener::from_std(listener)?,
        _path: Some(owned_path),
    })
}

pub struct ReadyPane {
    pub hello: Hello,
    pub socket: UnixStream,
}

pub async fn accept(socket: Option<&BoundSocket>) -> io::Result<UnixStream> {
    match socket {
        Some(socket) => socket.listener.accept().await.map(|(stream, _)| stream),
        None => std::future::pending().await,
    }
}

pub async fn handshake(mut socket: UnixStream) -> io::Result<ReadyPane> {
    let result = tokio::time::timeout(
        HELLO_TIMEOUT,
        Reader::new(&mut socket).read::<PaneToServer>(),
    )
    .await;
    match result {
        Ok(Ok(PaneToServer::Hello(hello)))
            if FrameSize::new(hello.width, hello.height).is_some() =>
        {
            Ok(ReadyPane { hello, socket })
        }
        _ => {
            let reason = "invalid or timed-out pane hello";
            let rejection = ServerToPane::Reject(reason.into());
            let _ = tokio::time::timeout(WRITE_TIMEOUT, send(&mut socket, &rejection)).await;
            Err(invalid(reason))
        }
    }
}

struct Waiting(Arc<AtomicBool>);
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

pub async fn control(socket: UnixStream, incoming: IncomingSender) -> io::Result<()> {
    let (mut rx, mut tx) = socket.into_split();
    let request: ControlRequest =
        tokio::time::timeout(Duration::from_secs(1), Reader::new(&mut rx).read()).await??;
    let (sender, answer) = oneshot::channel();
    let waiting = Waiting(Arc::new(AtomicBool::new(true)));
    let deadline = std::time::Instant::now() + CONTROL_TIMEOUT;
    let reply = Reply {
        sender,
        waiting: Arc::clone(&waiting.0),
        deadline,
    };
    let response = tokio::time::timeout_at(deadline.into(), async {
        incoming
            .send(Incoming::Control(request, reply))
            .await
            .map_err(io::Error::other)?;
        let mut eof = [0; 1];
        tokio::select! {
            response = answer => response.map_err(io::Error::other),
            result = rx.read(&mut eof) => {
                result?;
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "control client disconnected"))
            }
        }
    })
    .await??;
    tokio::time::timeout(WRITE_TIMEOUT, send(&mut tx, &response)).await??;
    Ok(())
}

/// A bounded output queue and cancellation authority for one connection task.
#[derive(Debug)]
pub struct Pane {
    output: Output,
    task: AbortHandle,
}

#[derive(Debug)]
enum Output {
    Open {
        sender: mpsc::Sender<ServerToPane>,
        release: oneshot::Sender<String>,
    },
    Draining,
}

impl Pane {
    pub fn start(
        id: PaneId,
        socket: UnixStream,
        incoming: IncomingSender,
        tasks: &mut JoinSet<PaneId>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(PANE_QUEUE);
        let (release, reason) = oneshot::channel();
        let task = tasks.spawn(async move {
            let (rx, mut tx) = socket.into_split();
            let incoming = async {
                let mut reader = Reader::new(rx);
                loop {
                    let message = reader.read().await?;
                    incoming
                        .send(Incoming::PaneMessage(id, message))
                        .await
                        .map_err(io::Error::other)?;
                }
            };
            let outgoing = pane_writer(&mut tx, receiver, reason);
            let result: io::Result<()> =
                tokio::select! { result = incoming => result, result = outgoing => result };
            if let Err(error) = result {
                tracing::debug!(?id, %error, "pane connection ended");
            }
            id
        });
        Self {
            output: Output::Open { sender, release },
            task,
        }
    }

    pub const fn active(&self) -> bool {
        matches!(self.output, Output::Open { .. })
    }

    pub fn task_id(&self) -> tokio::task::Id {
        self.task.id()
    }

    pub fn send(&self, message: ServerToPane) -> bool {
        match &self.output {
            Output::Open { sender, .. } => sender.try_send(message).is_ok(),
            Output::Draining => false,
        }
    }

    /// Drain queued messages before Release; never interrupt an active frame.
    pub fn release(&mut self, reason: String) {
        if let Output::Open { release, .. } = std::mem::replace(&mut self.output, Output::Draining)
        {
            let _ = release.send(reason);
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn pane_writer(
    writer: &mut (impl AsyncWrite + Unpin),
    mut messages: mpsc::Receiver<ServerToPane>,
    reason: oneshot::Receiver<String>,
) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    while let Some(message) = messages.recv().await {
        outgoing.send_pane(message)?;
        outgoing.flush_to(writer).await?;
    }
    if let Ok(reason) = reason.await {
        send(writer, &ServerToPane::Release(reason)).await?;
    }
    Ok(())
}

async fn send<T: serde::Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    outgoing.send(message)?;
    outgoing.flush_to(writer).await
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
