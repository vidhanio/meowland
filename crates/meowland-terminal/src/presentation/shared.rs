//! Retained read handles and consumption deadlines for upstream RGB objects.

use std::{
    fs::File,
    io::{self, Read},
    time::{Duration, Instant},
};

use meowland_protocol::{FrameBand, SharedFrame};

const TIMEOUT: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

pub(super) enum Consumption {
    Waiting(Instant),
    Complete,
    Expired,
}

pub(super) struct SharedPresentation {
    pub(super) frame: SharedFrame,
    file: File,
    deadline: Instant,
    pub(super) consumed: bool,
}

impl SharedPresentation {
    pub(super) fn new(frame: SharedFrame) -> io::Result<Self> {
        let file = frame.open()?;
        Ok(Self {
            frame,
            file,
            deadline: Instant::now() + TIMEOUT,
            consumed: false,
        })
    }

    pub(super) fn poll(&mut self) -> Consumption {
        match self.frame.exists() {
            Ok(false) => {
                self.consumed = true;
                Consumption::Complete
            }
            Ok(true) if Instant::now() < self.deadline => {
                Consumption::Waiting((Instant::now() + POLL_INTERVAL).min(self.deadline))
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
