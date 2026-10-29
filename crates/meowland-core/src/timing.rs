//! The virtual output's shared refresh policy.

use std::time::Duration;

use thiserror::Error;

/// A validated virtual output refresh rate between 1 and 1000 hertz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshRate(u16);

/// A refresh rate outside the supported 1 to 1000 hertz range.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("refresh rate must be between 1 and 1000 Hz")]
pub struct InvalidRefreshRate;

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
    type Error = InvalidRefreshRate;

    fn try_from(hertz: u32) -> Result<Self, Self::Error> {
        if !(1..=1000).contains(&hertz) {
            return Err(InvalidRefreshRate);
        }
        let hertz = u16::try_from(hertz).map_err(|_| InvalidRefreshRate)?;
        Ok(Self(hertz))
    }
}
