//! Bounded runtime latency samples with nearest-rank percentile summaries.

use std::time::Duration;

/// Maximum samples retained between summaries, including high-rate producers.
const MAX_SAMPLES: usize = 2048;

/// Microsecond latency percentiles for one reporting window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Percentiles {
    /// Median latency in microseconds.
    pub p50: u64,
    /// 95th-percentile latency in microseconds.
    pub p95: u64,
    /// 99th-percentile latency in microseconds.
    pub p99: u64,
}

/// A bounded rolling sample set whose allocation survives reporting windows.
#[derive(Debug, Default)]
pub struct Latency {
    samples: Vec<u64>,
    next: usize,
}

impl Latency {
    /// Retain a sample, replacing the oldest when the window is full.
    pub fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(micros);
        } else {
            self.samples[self.next] = micros;
            self.next = (self.next + 1) % MAX_SAMPLES;
        }
    }

    /// Summarize and reset the window, retaining its storage for reuse.
    pub fn summarize(&mut self) -> Percentiles {
        self.samples.sort_unstable();
        let percentile = |rank: usize| {
            let index = (self.samples.len() * rank).div_ceil(100).saturating_sub(1);
            self.samples.get(index).copied().unwrap_or_default()
        };
        let summary = Percentiles {
            p50: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
        };
        self.samples.clear();
        self.next = 0;
        summary
    }
}
