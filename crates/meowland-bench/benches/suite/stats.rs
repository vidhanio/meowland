use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Stats {
    pub fn from_samples(mut samples: Vec<Duration>) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let at = |fraction: f64| {
            let index = ((samples.len() as f64 * fraction) as usize).min(samples.len() - 1);
            samples[index]
        };
        Some(Self {
            p50: at(0.50),
            p95: at(0.95),
            p99: at(0.99),
            max: samples[samples.len() - 1],
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Rate {
    Fps,
    Ops,
    Bytes(u64),
}

#[derive(Debug)]
pub struct Row {
    pub label: String,
    pub count: usize,
    pub stats: Stats,
    pub elapsed: Duration,
    pub rate: Rate,
}

impl Row {
    pub fn new(
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

pub fn measure(
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
