//! Standalone microbenchmarks and end-to-end Wayland-to-terminal measurements.

mod codec;
mod pipeline;
mod session;
mod stats;

use std::io::{self, Write};
use std::time::Duration;
use std::{env, process};

use stats::{Rate, Row};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunLength {
    Quick,
    Full,
}

struct Options {
    length: RunLength,
    frames: Option<usize>,
    filter: Option<String>,
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        length: RunLength::Full,
        frames: None,
        filter: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--quick" => options.length = RunLength::Quick,
            "--frames" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--frames needs a value".to_owned())?;
                options.frames = Some(
                    value
                        .parse()
                        .map_err(|error| format!("invalid --frames {value:?}: {error}"))?,
                );
            }
            "--filter" => {
                options.filter = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--filter needs a value".to_owned())?,
                );
            }
            "--help" | "-h" => {
                writeln!(io::stdout().lock(),
                    "usage: cargo bench -p meowland-bench -- [--quick] [--frames N] [--filter TEXT]\n\
                     Set MEOWLAND_BENCH_SERVER=/path/to/meowland to benchmark the full server."
                ).map_err(|error| error.to_string())?;
                process::exit(0);
            }
            "--bench" => {}
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(options)
}

fn format_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.2} us", duration.as_secs_f64() * 1e6)
    } else if nanos < 1_000_000_000 {
        format!("{:.2} ms", duration.as_secs_f64() * 1e3)
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "human-readable benchmark rates intentionally approximate integer counters as f64"
)]
fn format_rate(row: &Row) -> String {
    let seconds = row.elapsed.as_secs_f64().max(f64::EPSILON);
    match row.rate {
        Rate::Fps => format!("{:.1} fps", row.count as f64 / seconds),
        Rate::Ops => format!("{:.0}/s", row.count as f64 / seconds),
        Rate::Bytes(bytes) => format!(
            "{:.2} GiB/s",
            bytes as f64 / seconds / (1024.0 * 1024.0 * 1024.0)
        ),
    }
}

fn print_rows(rows: &[Row]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "{:<56} {:>7} {:>11} {:>11} {:>11} {:>11} {:>12}",
        "scenario", "n", "p50", "p95", "p99", "max", "rate"
    )?;
    for row in rows {
        writeln!(
            stdout,
            "{:<56} {:>7} {:>11} {:>11} {:>11} {:>11} {:>12}",
            row.label,
            row.count,
            format_duration(row.stats.p50),
            format_duration(row.stats.p95),
            format_duration(row.stats.p99),
            format_duration(row.stats.max),
            format_rate(row),
        )?;
    }
    Ok(())
}

fn main() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error}");
            process::exit(2);
        }
    };
    let mut rows = codec::terminal(options.length);
    rows.extend(codec::wire(options.length));
    rows.extend(pipeline::run(&pipeline::Settings {
        length: options.length,
        frames: options.frames,
        filter: options.filter.clone(),
    }));
    if let Some(filter) = &options.filter {
        rows.retain(|row| row.label.contains(filter));
    }
    if rows.is_empty() {
        let _ = writeln!(io::stderr().lock(), "no benchmarks ran");
        process::exit(1);
    }
    if let Err(error) = print_rows(&rows) {
        let _ = writeln!(
            io::stderr().lock(),
            "cannot print benchmark results: {error}"
        );
        process::exit(1);
    }
}
