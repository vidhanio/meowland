//! Criterion suite over real Meowland process trees.
//!
//! Each case starts one isolated [`Session`] the first time Criterion runs
//! it and reuses that session for every warm-up and measurement batch, so
//! process startup never lands in a sample. Iterations report durations
//! taken from the workload's and endpoint's `CLOCK_MONOTONIC` timestamps,
//! never wall time around the loop, so fixture painting and harness
//! bookkeeping between operations are excluded.

use std::time::Duration;

use anyhow::{Context as _, ensure};
use criterion::{BenchmarkGroup, Criterion, SamplingMode, Throughput, measurement::WallTime};
use meowland_bench::{
    binaries::Binaries,
    control::{self, Command},
    endpoint::{Counters, Transport},
    pattern::Pattern,
    session::{Attach, Config, STEP_TIMEOUT, Session},
};

/// Operations run after startup and before the first batch.
const WARMUP: u64 = 3;
/// Interval between stream offers: 120 Hz.
const OFFER_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 120);

/// What one Criterion iteration is.
#[derive(Clone, Copy)]
enum Kind {
    /// Commit a frame and wait for it; the sample is commit to consumption.
    Latency,
    /// Write a key press and wait for the frame the workload commits in
    /// response; the sample is key write to consumption.
    Input,
    /// Recommit the displayed content unchanged; the sample is commit to the
    /// compositor's buffer release, and no image may be uploaded.
    Static,
    /// Offer frames at 120 Hz without waiting; an iteration is one frame
    /// consumed, so time per iteration is the mean interval between
    /// delivered frames. `throttled` limits terminal reads once warm.
    Stream { throttled: bool },
}

/// Transports a case's batches must use.
#[derive(Clone, Copy, Debug)]
enum Uploads {
    /// At least one shared-memory upload.
    Shared,
    /// Inline uploads only.
    Inline,
    /// No uploads at all.
    None,
}

struct Case {
    name: &'static str,
    kind: Kind,
    config: Config,
    uploads: Uploads,
    sample_size: usize,
    measurement: Duration,
}

const fn config(width: usize, height: usize, pattern: Pattern, transport: Transport) -> Config {
    Config {
        width,
        height,
        pattern,
        transport,
        attach: Attach::Launch,
    }
}

const fn case(
    name: &'static str,
    kind: Kind,
    config: Config,
    uploads: Uploads,
    sample_size: usize,
    measurement: u64,
) -> Case {
    Case {
        name,
        kind,
        config,
        uploads,
        sample_size,
        measurement: Duration::from_secs(measurement),
    }
}

/// The matrix. Streams come last because a group's throughput setting
/// cannot be cleared.
const CASES: [Case; 9] = [
    case(
        "full-1080p",
        Kind::Latency,
        config(1920, 1080, Pattern::Full, Transport::Shared),
        Uploads::Shared,
        30,
        5,
    ),
    case(
        "sparse-1080p",
        Kind::Latency,
        config(1920, 1080, Pattern::Sparse, Transport::Shared),
        Uploads::Inline,
        100,
        3,
    ),
    case(
        "input",
        Kind::Input,
        config(640, 360, Pattern::Sparse, Transport::Shared),
        Uploads::Inline,
        100,
        3,
    ),
    case(
        "static",
        Kind::Static,
        config(640, 360, Pattern::Sparse, Transport::Shared),
        Uploads::None,
        100,
        3,
    ),
    case(
        "inline",
        Kind::Latency,
        config(640, 360, Pattern::Full, Transport::Inline),
        Uploads::Inline,
        50,
        4,
    ),
    case(
        "full-4k",
        Kind::Latency,
        config(3840, 2160, Pattern::Full, Transport::Shared),
        Uploads::Shared,
        10,
        4,
    ),
    case(
        "resize",
        Kind::Latency,
        config(640, 360, Pattern::Resize, Transport::Shared),
        Uploads::Shared,
        50,
        4,
    ),
    case(
        "stream",
        Kind::Stream { throttled: false },
        config(1920, 1080, Pattern::Full, Transport::Shared),
        Uploads::Shared,
        10,
        3,
    ),
    case(
        "slow-inline",
        Kind::Stream { throttled: true },
        config(1920, 1080, Pattern::Full, Transport::Inline),
        Uploads::Inline,
        10,
        15,
    ),
];

/// A warm session and the next unused frame ID.
struct Runner {
    case: &'static Case,
    session: Session,
    next: u64,
    /// Frame the pane shows once warm, recommitted by [`Kind::Static`].
    shown: u64,
}

impl Runner {
    fn start(binaries: &Binaries, case: &'static Case) -> anyhow::Result<Self> {
        let mut session = Session::start(binaries, case.config)?;
        for frame in 1..=WARMUP {
            match case.kind {
                Kind::Input => session.input(frame)?,
                Kind::Latency | Kind::Static | Kind::Stream { .. } => session.frame(frame)?,
            };
        }
        session.settle(Duration::from_millis(50))?;
        if matches!(case.kind, Kind::Stream { throttled: true }) {
            session.throttle();
        }
        Ok(Self {
            case,
            session,
            next: WARMUP + 1,
            shown: WARMUP,
        })
    }

    fn measure(&mut self, iterations: u64) -> anyhow::Result<Duration> {
        let before = self.session.counters();
        let delivered = self.session.deliveries().len();
        let frames = self.next..self.next + iterations;
        let mut total = Duration::ZERO;
        match self.case.kind {
            Kind::Latency => {
                self.next = frames.end;
                for frame in frames {
                    total += self.session.frame(frame)?;
                }
            }
            Kind::Input => {
                self.next = frames.end;
                for frame in frames {
                    total += self.session.input(frame)?;
                }
            }
            Kind::Static => {
                self.next = frames.end;
                for frame in frames {
                    total += self.session.recommit(frame, self.shown)?;
                }
                self.session.settle(Duration::from_millis(30))?;
                ensure!(
                    self.session.deliveries().len() == delivered,
                    "unchanged commits produced a new frame"
                );
            }
            Kind::Stream { .. } => total = self.stream(iterations)?,
        }
        self.check(self.session.counters() - before)?;
        Ok(total)
    }

    /// Offer frames every [`OFFER_PERIOD`] until `count` of them are
    /// consumed, returning the time from the first offer to the last
    /// consumption, then let the newest offer drain outside the sample.
    fn stream(&mut self, count: u64) -> anyhow::Result<Duration> {
        let first = self.next;
        let mut seen = self.session.deliveries().len();
        let start = control::now();
        let (mut offered, mut consumed, mut progress) = (0_u32, 0, start);
        let end = loop {
            let mut end = None;
            for frame in &self.session.deliveries()[seen..] {
                if frame.id >= first {
                    consumed += 1;
                    progress = frame.at;
                    if consumed == count {
                        end = Some(frame.at);
                        break;
                    }
                }
            }
            seen = self.session.deliveries().len();
            if let Some(end) = end {
                break end;
            }
            let now = control::now();
            ensure!(
                now.saturating_sub(progress) < STEP_TIMEOUT,
                "no stream frame consumed for {STEP_TIMEOUT:?}"
            );
            let due = start + OFFER_PERIOD * offered;
            if now >= due {
                self.session
                    .command(Command::Commit(first + u64::from(offered)))?;
                offered += 1;
            }
            self.session
                .pump(due.saturating_sub(now).min(Duration::from_millis(1)))?;
        };
        self.next = first + u64::from(offered);
        let last = self.next - 1;
        self.session
            .until("stream drain", self.drain_timeout()?, |session| {
                session.committed(last).is_some() && session.consumed(last).is_some()
            })?;
        end.checked_sub(start)
            .context("stream frame consumed before the first offer")
    }

    /// Time allowed to drain the newest offer: one step, plus three times
    /// the throttled read time of an uncompressed frame.
    fn drain_timeout(&self) -> anyhow::Result<Duration> {
        let Config { width, height, .. } = self.case.config;
        let throttled = match self.case.kind {
            Kind::Stream { throttled: true } => width * height * 4 / 4096 * 2 * 3,
            _ => 0,
        };
        Ok(STEP_TIMEOUT + Duration::from_millis(throttled.try_into()?))
    }

    fn check(&self, uploads: Counters) -> anyhow::Result<()> {
        let (shared, inline) = (uploads.shared_uploads, uploads.inline_uploads);
        let ok = match self.case.uploads {
            Uploads::Shared => shared > 0,
            Uploads::Inline => shared == 0 && inline > 0,
            Uploads::None => shared + inline == 0,
        };
        ensure!(
            ok,
            "expected {:?} uploads, got {shared} shared and {inline} inline",
            self.case.uploads
        );
        Ok(())
    }
}

fn bench(group: &mut BenchmarkGroup<'_, WallTime>, binaries: &Binaries, case: &'static Case) {
    group
        .sample_size(case.sample_size)
        .measurement_time(case.measurement);
    if let Kind::Stream { .. } = case.kind {
        group
            .sampling_mode(SamplingMode::Flat)
            .throughput(Throughput::Elements(1));
    } else {
        group.sampling_mode(SamplingMode::Auto);
    }
    let mut runner = None;
    group.bench_function(case.name, |bencher| {
        bencher.iter_custom(|iterations| {
            let runner = runner.get_or_insert_with(|| {
                Runner::start(binaries, case)
                    .unwrap_or_else(|error| panic!("{}: {error:#}", case.name))
            });
            runner
                .measure(iterations)
                .unwrap_or_else(|error| panic!("{}: {error:#}", case.name))
        });
    });
}

fn suite(criterion: &mut Criterion, binaries: &Binaries) {
    let mut group = criterion.benchmark_group("pane");
    for case in &CASES {
        bench(&mut group, binaries, case);
    }
    group.finish();
}

fn main() {
    let binaries = Binaries::build(env!("CARGO_BIN_EXE_meowland-workload"))
        .expect("building meowland beside the workload");
    let mut criterion = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .configure_from_args();
    suite(&mut criterion, &binaries);
    criterion.final_summary();
}
