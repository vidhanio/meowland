//! Retained read handles, consumption deadlines, and background readback for
//! upstream RGB objects.

use std::{
    fs::File,
    io::{self, Read},
    mem,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedFrame};

/// How long to wait for the terminal to unlink a shared object before falling
/// back to inline transport.
const TIMEOUT: Duration = Duration::from_millis(250);
/// First consumption check delay. The terminal usually unlinks within one
/// frame, so a short first delay beats a fixed 1 ms loop.
const INITIAL_POLL_DELAY: Duration = Duration::from_millis(1);
/// Upper bound on the delay between consumption checks, keeping the worst-case
/// detection latency well inside [`TIMEOUT`].
const MAXIMUM_POLL_DELAY: Duration = Duration::from_millis(16);
/// Bytes of a consumed frame copied per scheduling turn.
///
/// Reading a full panel frame back is tens of megabytes of page faults. Doing
/// it in one go stalls the turn that accepts the next inline band, so the read
/// is spread across turns while the terminal would otherwise be idle. The size
/// trades turns for latency: 256 KiB finishes a 4K frame in about a hundred
/// turns, well before an inline band usually follows.
const READ_CHUNK: usize = 256 * 1024;

/// A shared frame shown by reference that the terminal has not read yet.
pub(super) struct Unread {
    frame: SharedFrame,
    file: File,
    deadline: Instant,
    poll_delay: Duration,
    next_poll: Instant,
}

/// The outcome of checking whether the terminal read a shared frame.
pub(super) enum Polled {
    /// Still unread; check again at [`Unread::next_poll`].
    Unread(Unread),
    /// The terminal unlinked the object; its pixels are being retained.
    Consumed(Readback),
    /// The terminal did not read the object in time.
    Expired(Unread),
}

impl Unread {
    /// Open the object so its pixels stay readable after the terminal unlinks
    /// it, and poll for consumption immediately.
    pub(super) fn new(frame: SharedFrame, now: Instant) -> io::Result<Self> {
        let file = frame.open()?;
        Ok(Self {
            frame,
            file,
            deadline: now + TIMEOUT,
            poll_delay: INITIAL_POLL_DELAY,
            next_poll: now,
        })
    }

    pub(super) const fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    pub(super) const fn next_poll(&self) -> Instant {
        self.next_poll
    }

    /// Check consumption, backing off so a slow terminal costs few syscalls.
    pub(super) fn poll(mut self, now: Instant) -> Polled {
        match self.frame.exists() {
            Ok(false) => Polled::Consumed(Readback::new(self.frame.size(), self.file)),
            Ok(true) if now < self.deadline => {
                self.next_poll = (now + self.poll_delay).min(self.deadline);
                self.poll_delay = (self.poll_delay * 2).min(MAXIMUM_POLL_DELAY);
                Polled::Unread(self)
            }
            Ok(true) => Polled::Expired(self),
            Err(error) => {
                tracing::warn!(%error, "checking upstream shared-frame consumption failed");
                Polled::Expired(self)
            }
        }
    }

    /// Read the whole frame synchronously.
    ///
    /// # Errors
    /// Returns an error if the shared object cannot be read or does not form a
    /// whole frame.
    pub(super) fn into_band(self) -> io::Result<FrameBand> {
        Readback::new(self.frame.size(), self.file).into_band()
    }
}

/// How much of a consumed frame has been copied out of the shared object.
pub(super) enum Readback {
    /// `pixels[..offset]` came from the object.
    Reading {
        file: File,
        size: FrameSize,
        pixels: Vec<u8>,
        offset: usize,
    },
    /// A complete frame, ready for the presenter without another read.
    Ready(FrameBand),
}

impl Readback {
    fn new(size: FrameSize, file: File) -> Self {
        Self::Reading {
            file,
            size,
            pixels: vec![0; size.rgb_len()],
            offset: 0,
        }
    }

    /// Whether the whole frame already lives in memory.
    pub(super) const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// Copy one bounded chunk into memory.
    ///
    /// Called once per scheduling turn after the terminal confirms consumption,
    /// so the next inline band finds the retained pixels ready instead of
    /// reading the whole object on its own critical path.
    ///
    /// # Errors
    /// Returns an error if the shared object cannot be read.
    pub(super) fn step(&mut self) -> io::Result<()> {
        let Self::Reading {
            file,
            size,
            pixels,
            offset,
        } = self
        else {
            return Ok(());
        };
        let end = (*offset + READ_CHUNK).min(pixels.len());
        file.read_exact(&mut pixels[*offset..end])?;
        *offset = end;
        if end == pixels.len() {
            let band = whole_band(*size, mem::take(pixels))?;
            *self = Self::Ready(band);
        }
        Ok(())
    }

    /// Finish the frame, reading any remainder synchronously.
    ///
    /// # Errors
    /// Returns an error if the shared object cannot be read or does not form a
    /// whole frame.
    pub(super) fn into_band(self) -> io::Result<FrameBand> {
        match self {
            Self::Ready(band) => Ok(band),
            Self::Reading {
                mut file,
                size,
                mut pixels,
                offset,
            } => {
                file.read_exact(&mut pixels[offset..])?;
                whole_band(size, pixels)
            }
        }
    }
}

fn whole_band(size: FrameSize, pixels: Vec<u8>) -> io::Result<FrameBand> {
    FrameBand::new(size, 0, 0, size.width(), pixels)
        .ok_or_else(|| io::Error::other("invalid retained shared frame"))
}
