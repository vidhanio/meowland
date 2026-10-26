//! The virtual output's shared refresh policy.

use std::time::Duration;

/// Virtual output refresh rate advertised to Wayland clients, in millihertz.
pub const REFRESH_MILLIHZ: i32 = 120_000;
/// Minimum interval between compositor frames at the advertised 120 Hz refresh
/// rate.
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 120);
