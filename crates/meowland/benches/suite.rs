//! End-to-end benchmarks using real server/pane processes and a headless PTY
//! terminal.
//!
//! There is no direct compositor or pane-protocol injection. Frame IDs survive
//! Wayland composition and Kitty placement; input samples require matching
//! transitions and an independently observed response frame. This is not a
//! measurement of terminal GPU rendering or physical input-to-photon latency.

mod support;

use std::io::Write;

use support::{config, scenarios};

pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type Result<T> = std::result::Result<T, Error>;
pub(crate) fn fail(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

fn run() -> Result<()> {
    let Some(options) = config::parse()? else {
        return Ok(());
    };
    let workloads: Vec<_> = scenarios::all()
        .into_iter()
        .filter(|scenario| scenario.name.contains(&options.filter))
        .collect();
    if workloads.is_empty() {
        return Err(fail("no workloads match --filter"));
    }
    writeln!(
        std::io::stdout().lock(),
        "binary={} transport={:?} samples={} warmup={} runs={} terminal_delay_ms={}\nBoundary: PTY injection/Wayland commit -> real server/pane -> headless RGB consumption. No GPU/display timing.\nRates include workload pacing and drain; coalesced IDs are excluded from latency samples.\nUploads count images, including background load; RGB denotes inline image transfers.\n{:29} {:10} {:>3} {:>11} {:>9} {:>9} {:>9} {:>9} {:>8} {:>5} {:>5} {:>11}",
        options.binary.display(),
        options.transport,
        options.samples,
        options.warmup,
        options.runs,
        options.terminal_delay.as_millis(),
        "workload",
        "phase",
        "run",
        "seen/sent",
        "p50 ms",
        "p95 ms",
        "p99 ms",
        "max ms",
        "seen/s",
        "skip",
        "late",
        "SHM/RGB",
    )?;
    let mut csv = options
        .csv
        .as_ref()
        .map(std::fs::File::create)
        .transpose()?;
    if let Some(csv) = &mut csv {
        writeln!(
            csv,
            "scenario,phase,run,observed,submitted,p50_ns,p95_ns,p99_ns,max_ns,elapsed_ns,observed_per_second,coalesced,late_offers,shared_uploads,inline_uploads"
        )?;
    }
    for scenario in workloads {
        for run in 1..=options.runs {
            let rows = scenarios::run(&scenario, &options, run)
                .map_err(|error| fail(format!("{} run {run}: {error}", scenario.name)))?;
            for mut row in rows {
                row.print(&mut csv)?;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(std::io::stderr().lock(), "benchmark failed: {error}");
        std::process::exit(1);
    }
}
