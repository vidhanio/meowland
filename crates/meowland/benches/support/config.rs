use std::{env, io, path::PathBuf, time::Duration};

use usage_rs::{Cli, ValueEnum};

use crate::{Result, fail};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Size {
    pub width: usize,
    pub height: usize,
}

impl Size {
    pub(crate) const HD: Self = Self {
        width: 1920,
        height: 1080,
    };
    pub(crate) const UHD: Self = Self {
        width: 3840,
        height: 2160,
    };

    pub(crate) const fn pixels(self) -> usize {
        self.width * self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Transport {
    Shared,
    Inline,
}

#[derive(Cli)]
#[usage(
    name = "meowland-bench",
    after_help = "Run: cargo bench -p meowland --bench suite -- [options]\n\nBoundary: real Wayland client -> server -> pane -> PTY -> headless Kitty consumer.\nInput starts at PTY injection; GPU upload, physical display, and hardware input are excluded.\nThroughput is offered at 120 Hz; skipped frame IDs are reported, never reassigned.\nEach operation has a 10-second timeout."
)]
struct Args {
    /// Real server/pane executable; defaults to `MEOWLAND_BENCH_BINARY` or the
    /// Cargo-built binary.
    #[usage(long)]
    binary: Option<PathBuf>,
    /// Use 12 samples, 3 warmups, and 1 run unless explicitly overridden.
    #[usage(long)]
    quick: bool,
    /// Samples or offered source frames per run (default 120).
    #[usage(long)]
    samples: Option<usize>,
    /// Unrecorded operations per run (default 8).
    #[usage(long)]
    warmup: Option<usize>,
    /// Independent process runs (default 3).
    #[usage(long)]
    runs: Option<usize>,
    /// Select workload names containing this text, before setup.
    #[usage(long)]
    filter: Option<String>,
    /// Advertised terminal graphics transport.
    #[usage(long, value_enum, default = "shared")]
    transport: Transport,
    /// Sleep before each PTY output read, in milliseconds (maximum 1000).
    #[usage(long, default = "0")]
    terminal_delay_ms: u64,
    /// Write per-run CSV summaries.
    #[usage(long)]
    csv: Option<PathBuf>,
    /// List workloads without starting any processes.
    #[usage(long)]
    list: bool,
    #[usage(long = "bench", hide)]
    _bench: bool,
}

pub(crate) struct Options {
    pub binary: PathBuf,
    pub samples: usize,
    pub warmup: usize,
    pub runs: usize,
    pub filter: String,
    pub transport: Transport,
    pub terminal_delay: Duration,
    pub csv: Option<PathBuf>,
}

pub(crate) fn parse() -> Result<Option<Options>> {
    let args = Args::parse();
    if args.list {
        use io::Write as _;
        for scenario in super::scenarios::all() {
            writeln!(io::stdout().lock(), "{}", scenario.name)?;
        }
        return Ok(None);
    }
    let options = Options {
        binary: args
            .binary
            .or_else(|| env::var_os("MEOWLAND_BENCH_BINARY").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meowland"))),
        samples: args.samples.unwrap_or(if args.quick { 12 } else { 120 }),
        warmup: args.warmup.unwrap_or(if args.quick { 3 } else { 8 }),
        runs: args.runs.unwrap_or(if args.quick { 1 } else { 3 }),
        filter: args.filter.unwrap_or_default(),
        transport: args.transport,
        terminal_delay: Duration::from_millis(args.terminal_delay_ms),
        csv: args.csv,
    };
    if options.samples == 0
        || options.runs == 0
        || options.samples > 100_000
        || options.warmup > 100_000
        || options.runs > 100
        || args.terminal_delay_ms > 1000
    {
        return Err(fail(
            "samples/runs must be nonzero; maximum samples/warmup 100000, runs 100, terminal delay 1000 ms",
        ));
    }
    let binary = options.binary.canonicalize().map_err(|error| {
        fail(format!(
            "{}: {error}; build with cargo build --release -p meowland",
            options.binary.display()
        ))
    })?;
    Ok(Some(Options { binary, ..options }))
}
