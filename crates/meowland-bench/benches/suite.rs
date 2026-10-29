//! Cargo entry point for isolated, external-process benchmarks.

use std::{
    env, io,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> io::Result<()> {
    let workload = PathBuf::from(env!("CARGO_BIN_EXE_meowland-workload"));
    let profile = workload
        .parent()
        .ok_or_else(|| io::Error::other("workload executable has no parent directory"))?;
    let target = profile
        .parent()
        .ok_or_else(|| io::Error::other("profile directory has no parent directory"))?;
    build_meowland(target, profile)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let error = Command::new("python3")
        .arg(root.join("benchmarks/run.py"))
        .args(env::args_os().skip(1))
        .current_dir(&root)
        .env("MEOWLAND_BENCH_BINARY", profile.join("meowland"))
        .env("MEOWLAND_BENCH_WORKLOAD", &workload)
        .exec();
    Err(error)
}

/// Build the released `meowland` binary beside the benchmark executables.
fn build_meowland(target: &Path, profile: &Path) -> io::Result<()> {
    let profile = profile
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("profile directory name is not UTF-8"))?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "build",
            "-p",
            "meowland",
            "--bin",
            "meowland",
            "--target-dir",
        ])
        .arg(target);
    match profile {
        "release" => {
            command.arg("--release");
        }
        "debug" => {}
        name => {
            command.args(["--profile", name]);
        }
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "building meowland failed: {status}"
        )))
    }
}
