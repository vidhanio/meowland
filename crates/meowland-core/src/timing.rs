//! The virtual output's shared refresh policy.

use std::{io, time::Duration};

/// A validated virtual output refresh rate between 1 and 1000 hertz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshRate(u16);

impl Default for RefreshRate {
    fn default() -> Self {
        Self(120)
    }
}

impl RefreshRate {
    /// Refresh rate advertised to Wayland clients, in millihertz.
    pub fn millihertz(self) -> i32 {
        i32::from(self.0) * 1000
    }

    /// Minimum interval between visible clients' frame callbacks.
    pub fn interval(self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.0))
    }
}

impl TryFrom<u32> for RefreshRate {
    type Error = io::Error;

    fn try_from(hertz: u32) -> Result<Self, Self::Error> {
        if !(1..=1000).contains(&hertz) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refresh rate must be between 1 and 1000 Hz",
            ));
        }
        u16::try_from(hertz).map(Self).map_err(io::Error::other)
    }
}
