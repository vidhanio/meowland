use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stats {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Stats {
    pub(crate) fn from_samples(mut samples: Vec<Duration>) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let at = |percent: usize| {
            let index = samples.len() / 100 * percent + samples.len() % 100 * percent / 100;
            samples[index.min(samples.len() - 1)]
        };
        Some(Self {
            p50: at(50),
            p95: at(95),
            p99: at(99),
            max: samples[samples.len() - 1],
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Rate {
    Fps,
    Ops,
    Bytes(u64),
}

impl Rate {
    pub(crate) fn bytes(bytes: usize, iterations: usize) -> Self {
        let bytes = u64::try_from(bytes).expect("benchmark byte count fits u64");
        let iterations = u64::try_from(iterations).expect("benchmark iteration count fits u64");
        Self::Bytes(
            bytes
                .checked_mul(iterations)
                .expect("benchmark byte total fits u64"),
        )
    }
}

#[derive(Debug)]
pub(crate) struct Row {
    pub label: String,
    pub count: usize,
    pub stats: Stats,
    pub elapsed: Duration,
    pub rate: Rate,
}

impl Row {
    pub(crate) fn new(
        label: impl Into<String>,
        count: usize,
        samples: Vec<Duration>,
        elapsed: Duration,
        rate: Rate,
    ) -> Option<Self> {
        Some(Self {
            label: label.into(),
            count,
            stats: Stats::from_samples(samples)?,
            elapsed,
            rate,
        })
    }
}

pub(crate) fn measure(
    warmup: usize,
    iterations: usize,
    mut operation: impl FnMut(),
) -> (Vec<Duration>, Duration) {
    for _ in 0..warmup {
        operation();
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let sample = Instant::now();
        operation();
        samples.push(sample.elapsed());
    }
    let elapsed = samples.iter().sum();
    (samples, elapsed)
}
