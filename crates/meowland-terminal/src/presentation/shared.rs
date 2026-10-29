//! Retained read handles and consumption deadlines for upstream RGB objects.

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

pub(super) enum Consumption {
    Waiting(Instant),
    Complete,
    Expired,
}

pub(super) struct SharedPresentation {
    pub(super) frame: SharedFrame,
    file: File,
    deadline: Instant,
    poll_delay: Duration,
    pub(super) consumed: bool,
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

    pub(super) fn into_band(mut self) -> io::Result<FrameBand> {
        let mut pixels = vec![0; self.frame.size().rgb_len()];
        self.file.read_exact(&mut pixels)?;
        FrameBand::new(self.frame.size(), 0, pixels)
            .ok_or_else(|| io::Error::other("invalid retained shared frame"))
    }
}
