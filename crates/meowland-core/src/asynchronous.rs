//! Async readiness for exclusively owned, nonblocking file descriptors.

use std::{
    io,
    os::fd::OwnedFd,
    pin::Pin,
    task::{Context, Poll, ready},
};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

/// An owned descriptor registered with the current Tokio reactor.
#[derive(Debug)]
pub struct Descriptor(AsyncFd<OwnedFd>);

impl Descriptor {
    /// Enable nonblocking I/O and register the descriptor.
    ///
    /// The caller must use an independent open file description when its
    /// blocking flags must not affect another user of the same file.
    ///
    /// # Errors
    /// Returns an error if nonblocking mode or reactor registration fails.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
        // SAFETY: The private OwnedFd stays open and unchanged until AsyncFd
        // drops it.
        unsafe { AsyncFd::register(fd) }
            .map(Self)
            .map_err(Into::into)
    }
}

impl AsyncRead for Descriptor {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let mut ready = ready!(self.0.poll_read_ready(cx))?;
            if let Ok(result) = ready.try_io(|fd| {
                rustix::io::read(fd, buffer.initialize_unfilled()).map_err(io::Error::from)
            }) {
                match result {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => buffer.advance(result?),
                }
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncWrite for Descriptor {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = ready!(self.0.poll_write_ready(cx))?;
            if let Ok(result) =
                ready.try_io(|fd| rustix::io::write(fd, bytes).map_err(io::Error::from))
            {
                match result {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => return Poll::Ready(result),
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
