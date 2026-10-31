//! Asynchronous terminal input descriptor.

use std::{io, time::Instant};

use calloop::LoopHandle;
use futures::io::AsyncReadExt;
use meowland_async::asynchronous::Descriptor;
use rustix::fs::{Mode, OFlags, open};

use super::{event::Event, parser::Parser};

/// Nonblocking terminal input descriptor with incremental, bounded VT decoding.
pub struct Reader {
    fd: Descriptor,
    parser: Parser,
    bytes: Box<[u8]>,
}

impl Reader {
    /// Open terminal input without changing stdin's blocking flags.
    ///
    /// # Errors
    /// Returns an error if opening, registering, or watching input fails.
    pub fn new<D>(handle: &LoopHandle<'static, D>) -> io::Result<Self> {
        let fd = open(
            "/proc/self/fd/0",
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        let fd = Descriptor::new(fd, handle)?;
        Ok(Self {
            fd,
            parser: Parser::new(),
            bytes: vec![0; 16 * 1024].into_boxed_slice(),
        })
    }

    /// Whether decoded events are ready without another descriptor read.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.parser.pending()
    }

    /// Deadline by which a muted control sequence must return to ground.
    #[must_use]
    pub const fn recovery_deadline(&self) -> Option<Instant> {
        self.parser.recovery_deadline()
    }

    /// Fail if a muted control sequence has exceeded its recovery deadline.
    ///
    /// # Errors
    /// Returns an input error when the recovery deadline has elapsed.
    pub fn check_recovery(&self) -> io::Result<()> {
        self.parser.check_recovery_now()?;
        Ok(())
    }

    /// Pop a decoded terminal event.
    pub fn next_event(&mut self) -> Option<Event> {
        self.parser.next_event()
    }

    /// Read and decode the next bounded input chunk.
    ///
    /// # Errors
    /// Returns an error on EOF, input failure, or an unterminated control
    /// sequence that exceeds the recovery limit, including one whose deadline
    /// elapsed while this read was waiting.
    #[expect(
        clippy::future_not_send,
        reason = "terminal input belongs to the calloop owner thread"
    )]
    pub async fn read_ready(&mut self) -> io::Result<()> {
        self.check_recovery()?;
        if self.parser.pending() {
            return Ok(());
        }
        let read = self.fd.read(&mut self.bytes).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.check_recovery()?;
        for &byte in &self.bytes[..read] {
            self.parser.push(byte)?;
        }
        Ok(())
    }

    /// Read raw capability replies before starting event decoding.
    ///
    /// # Errors
    /// Returns an error if terminal input fails.
    #[expect(
        clippy::future_not_send,
        reason = "terminal input belongs to the calloop owner thread"
    )]
    pub async fn read_raw(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.fd.read(bytes).await
    }
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::Write,
        os::unix::net::UnixStream,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    use super::*;
    use crate::input::parser::tests::muted_parser;

    #[test]
    fn reader_fails_closed_when_a_read_completes_after_the_recovery_deadline() {
        fn poll_once(
            future: &mut Pin<Box<impl Future<Output = io::Result<()>>>>,
        ) -> Poll<io::Result<()>> {
            let mut context = Context::from_waker(Waker::noop());
            future.as_mut().poll(&mut context)
        }

        let event_loop: calloop::EventLoop<'static, ()> =
            calloop::EventLoop::try_new().expect("event loop");
        let (read, mut write) = UnixStream::pair().expect("socket pair");
        let fd = Descriptor::new(read.into(), &event_loop.handle()).expect("descriptor");
        let mut reader = Reader {
            fd,
            parser: muted_parser(),
            bytes: vec![0; 16 * 1024].into_boxed_slice(),
        };
        let clock = reader.parser.clock().clone();
        let deadline = reader
            .parser
            .recovery_deadline()
            .expect("muted after mouse overflow");

        let mut read_ready = Box::pin(reader.read_ready());
        assert!(
            poll_once(&mut read_ready).is_pending(),
            "the read must wait for input before the deadline"
        );
        clock.freeze(deadline);
        write.write_all(b"1").expect("write a continuation byte");
        let result = match poll_once(&mut read_ready) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("the read must complete after the continuation byte"),
        };
        let error = result.expect_err("a read completing after the recovery deadline must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(read_ready);
        assert!(reader.parser.recovery_deadline().is_some());
        assert!(!reader.pending());
    }
}
