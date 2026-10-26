//! The virtual output's shared refresh policy.

use std::time::Duration;

pub const REFRESH_MILLIHZ: i32 = 120_000;
pub const FRAME_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000_000 / REFRESH_MILLIHZ as u64);
