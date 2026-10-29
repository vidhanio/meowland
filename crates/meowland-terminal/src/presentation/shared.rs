//! Retained read handles, consumption deadlines, and background readback for
//! upstream RGB objects.

use std::{
    fs::File,
    io::{self, Read},
    time::{Duration, Instant},
};

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

pub(super) enum Consumption {
    Waiting(Instant),
    Complete,
    Expired,
}

/// How much of a consumed frame has been copied out of the shared object.
enum Readback {
    /// Nothing copied yet: the terminal may still be reading the object.
    Idle,
    /// `pixels[..offset]` came from the object.
    Reading { pixels: Vec<u8>, offset: usize },
    /// A complete frame, ready for the presenter without another read.
    Ready(FrameBand),
}

pub(super) struct SharedPresentation {
    pub(super) frame: SharedFrame,
    file: File,
    deadline: Instant,
    poll_delay: Duration,
    pub(super) consumed: bool,
    readback: Readback,
}

impl SharedPresentation {
    pub(super) fn new(frame: SharedFrame) -> io::Result<Self> {
        let file = frame.open()?;
        Ok(Self {
            frame,
            file,
            deadline: Instant::now() + TIMEOUT,
            poll_delay: INITIAL_POLL_DELAY,
            consumed: false,
            readback: Readback::Idle,
        })
    }

    /// Check consumption, backing off so a slow terminal costs few syscalls.
    pub(super) fn poll(&mut self) -> Consumption {
        match self.frame.exists() {
            Ok(false) => {
                self.consumed = true;
                Consumption::Complete
            }
            Ok(true) if Instant::now() < self.deadline => {
                let next = (Instant::now() + self.poll_delay).min(self.deadline);
                self.poll_delay = (self.poll_delay * 2).min(MAXIMUM_POLL_DELAY);
                Consumption::Waiting(next)
            }
            Ok(true) => Consumption::Expired,
            Err(error) => {
                tracing::warn!(%error, "checking upstream shared-frame consumption failed");
                Consumption::Expired
            }
        }
    }

    /// Whether the whole frame already lives in memory.
    pub(super) const fn retained(&self) -> bool {
        matches!(self.readback, Readback::Ready(_))
    }

    /// Copy one bounded chunk of a consumed frame, reporting completion.
    ///
    /// Called once per scheduling turn after the terminal confirms consumption,
    /// so the next inline band finds the retained pixels ready instead of
    /// reading the whole object on its own critical path.
    ///
    /// # Errors
    /// Returns an error if the shared object cannot be read.
    pub(super) fn retain_step(&mut self) -> io::Result<bool> {
        if self.retained() {
            return Ok(true);
        }
        if !self.consumed {
            return Ok(false);
        }
        if matches!(self.readback, Readback::Idle) {
            self.readback = Readback::Reading {
                pixels: vec![0; self.frame.size().rgb_len()],
                offset: 0,
            };
        }
        let Readback::Reading { pixels, offset } = &mut self.readback else {
            unreachable!("a consumed frame is either being read or already retained");
        };
        let end = (*offset + READ_CHUNK).min(pixels.len());
        self.file.read_exact(&mut pixels[*offset..end])?;
        *offset = end;
        if *offset < pixels.len() {
            return Ok(false);
        }
        let Readback::Reading { pixels, .. } =
            std::mem::replace(&mut self.readback, Readback::Idle)
        else {
            unreachable!("the readback was just filled");
        };
        self.readback = Readback::Ready(whole_band(&self.frame, pixels)?);
        Ok(true)
    }

    /// Finish the frame, reading any remainder synchronously.
    ///
    /// # Errors
    /// Returns an error if the shared object cannot be read or does not form a
    /// whole frame.
    pub(super) fn into_band(mut self) -> io::Result<FrameBand> {
        let size = self.frame.size();
        let (mut pixels, offset) = match std::mem::replace(&mut self.readback, Readback::Idle) {
            Readback::Ready(band) => return Ok(band),
            Readback::Reading { pixels, offset } => (pixels, offset),
            Readback::Idle => (vec![0; size.rgb_len()], 0),
        };
        self.file.read_exact(&mut pixels[offset..])?;
        whole_band(&self.frame, pixels)
    }
}

fn whole_band(frame: &SharedFrame, pixels: Vec<u8>) -> io::Result<FrameBand> {
    let size = frame.size();
    FrameBand::new(size, 0, 0, size.width(), pixels)
        .ok_or_else(|| io::Error::other("invalid retained shared frame"))
}
