//! Cargo entry point for isolated, external-process benchmarks.

use std::{os::unix::process::CommandExt as _, process::Command};

fn main() -> std::io::Result<()> {
    let error = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../benchmarks/run.py"
        ))
        .args(std::env::args_os().skip(1))
        .env("MEOWLAND_BENCH_BINARY", env!("CARGO_BIN_EXE_meowland"))
        .env(
            "MEOWLAND_BENCH_WORKLOAD",
            env!("CARGO_BIN_EXE_meowland-workload"),
        )
        .exec();
    Err(error)
}
