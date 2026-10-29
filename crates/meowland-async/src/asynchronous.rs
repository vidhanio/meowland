//! Local asynchronous I/O and deadlines on a caller-owned calloop event loop.

use std::{
    fs::File,
    io,
    os::fd::OwnedFd,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use calloop::{
    LoopHandle, RegistrationToken,
    io::Async,
    timer::{TimeoutAction, Timer},
};
use futures::{
    FutureExt,
    channel::oneshot,
    io::{AsyncRead, AsyncWrite},
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

/// An exclusively owned nonblocking descriptor registered on the caller's loop.
#[derive(Debug)]
pub struct Descriptor(Async<'static, File>);

impl Descriptor {
    /// Enable nonblocking I/O without sharing the owner's mutable state.
    ///
    /// The descriptor must have an independent open file description if changes
    /// to its blocking flags would affect another user.
    ///
    /// # Errors
    /// Returns an error if nonblocking mode or readiness registration fails.
    pub fn new<D>(fd: OwnedFd, handle: &LoopHandle<'static, D>) -> io::Result<Self> {
        fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
        handle
            .adapt_io(File::from(fd))
            .map(Self)
            .map_err(Into::into)
    }
}

impl AsyncRead for Descriptor {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, bytes)
    }
}

impl AsyncWrite for Descriptor {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}

struct Registration<D> {
    handle: LoopHandle<'static, D>,
    token: RegistrationToken,
}

impl<D> Drop for Registration<D> {
    fn drop(&mut self) {
        self.handle.remove(self.token);
    }
}

/// Sleep until a deadline, unregistering the timer on cancellation.
///
/// `None` waits indefinitely without registering a timer.
///
/// # Errors
/// Returns an error if the timer cannot be registered.
#[expect(
    clippy::future_not_send,
    reason = "calloop futures stay on the owner thread"
)]
pub async fn sleep_until<D: 'static>(
    handle: &LoopHandle<'static, D>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    let (sender, receiver) = oneshot::channel();
    let mut sender = Some(sender);
    let token = handle
        .insert_source(Timer::from_deadline(deadline), move |_, (), _| {
            if let Some(sender) = sender.take() {
                let _ = sender.send(());
            }
            TimeoutAction::Drop
        })
        .map_err(|error| io::Error::from(error.error))?;
    let _registration = Registration {
        handle: handle.clone(),
        token,
    };
    receiver.await.map_err(io::Error::other)
}

/// Bound an operation using the caller's loop, cancelling the losing future.
///
/// # Errors
/// Returns a timeout, registration failure, or the operation's error.
#[expect(
    clippy::future_not_send,
    reason = "calloop futures stay on the owner thread"
)]
pub async fn timeout<D: 'static, T>(
    handle: &LoopHandle<'static, D>,
    duration: std::time::Duration,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    futures::select_biased! {
        result = operation.fuse() => result,
        result = sleep_until(handle, Some(Instant::now() + duration)).fuse() => {
            result?;
            Err(io::ErrorKind::TimedOut.into())
        }
    }
}

/// Poll an operation only when enabled, otherwise wait without waking the loop.
pub async fn when<T>(operation: Option<impl Future<Output = T>>) -> T {
    match operation {
        Some(operation) => operation.await,
        None => std::future::pending().await,
    }
}

/// Connect a nonblocking Unix socket without a runtime-specific reactor.
///
/// # Errors
/// Returns an error if socket creation, registration, or connection fails.
#[expect(
    clippy::future_not_send,
    reason = "calloop futures stay on the owner thread"
)]
pub async fn connect<D: 'static>(
    handle: &LoopHandle<'static, D>,
    path: &std::path::Path,
) -> io::Result<Async<'static, std::os::unix::net::UnixStream>> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    let address = SocketAddrUnix::new(path)?;
    let pending = match rustix::net::connect(&fd, &address) {
        Ok(()) => false,
        Err(rustix::io::Errno::INPROGRESS) => true,
        Err(error) => return Err(error.into()),
    };
    let mut socket = handle.adapt_io(std::os::unix::net::UnixStream::from(fd))?;
    if pending {
        socket.writable().await;
        if let Some(error) = socket.get_mut().take_error()? {
            return Err(error);
        }
    }
    Ok(socket)
}
