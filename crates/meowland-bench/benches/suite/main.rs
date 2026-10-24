mod codec;
mod pipeline;
mod stats;

use std::{env, process, time::Duration};

use stats::{Rate, Row};

struct Options {
    quick: bool,
    frames: Option<usize>,
    filter: Option<String>,
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        quick: false,
        frames: None,
        filter: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--quick" => options.quick = true,
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
                println!(
                    "usage: cargo bench -p meowland-bench -- [--quick] [--frames N] [--filter TEXT]"
                );
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

fn format_rate(row: &Row) -> String {
    let seconds = row.elapsed.as_secs_f64().max(f64::EPSILON);
    match row.rate {
        Rate::Fps => format!("{:.1} fps", row.count as f64 / seconds),
        Rate::Ops => format!("{:.0}/s", row.count as f64 / seconds),
        Rate::Bytes(bytes) => format!("{:.2} GiB/s", bytes as f64 / seconds / 1e9),
    }
}

fn print_rows(rows: &[Row]) {
    println!(
        "{:<56} {:>7} {:>11} {:>11} {:>11} {:>11} {:>12}",
        "scenario", "n", "p50", "p95", "p99", "max", "rate"
    );
    for row in rows {
        println!(
            "{:<56} {:>7} {:>11} {:>11} {:>11} {:>11} {:>12}",
            row.label,
            row.count,
            format_duration(row.stats.p50),
            format_duration(row.stats.p95),
            format_duration(row.stats.p99),
            format_duration(row.stats.max),
            format_rate(row),
        );
    }
}

fn main() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            process::exit(2);
        }
    };
    let mut rows = codec::terminal(options.quick);
    rows.extend(codec::wire(options.quick));
    rows.extend(pipeline::run(&pipeline::Settings {
        quick: options.quick,
        frames: options.frames,
    }));
    if let Some(filter) = &options.filter {
        rows.retain(|row| row.label.contains(filter));
    }
    if rows.is_empty() {
        eprintln!("no benchmarks ran");
        process::exit(1);
    }
    print_rows(&rows);
}
