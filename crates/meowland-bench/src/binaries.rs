//! Executables the harness runs.

use std::{
    env, io,
    path::{Path, PathBuf},
    process::Command,
};

/// The `meowland` and `meowland-workload` executables of one Cargo profile.
#[derive(Clone, Debug)]
pub struct Binaries {
    /// The compositor and its command-line client.
    pub meowland: PathBuf,
    /// The SHM workload fixture.
    pub workload: PathBuf,
}

impl Binaries {
    /// Build `meowland` beside `workload`, with the profile and target
    /// directory that produced it.
    ///
    /// Benches and integration tests only get `CARGO_BIN_EXE_*` for their own
    /// package's binaries, and Cargo cannot depend on another package's
    /// binary on stable, so this runs `cargo build -p meowland` (a no-op when
    /// it is current) rather than trusting a possibly stale executable.
    ///
    /// # Errors
    /// Returns an error if the path does not sit in a Cargo profile directory
    /// or the build fails.
    pub fn build(workload: impl Into<PathBuf>) -> io::Result<Self> {
        let workload = workload.into();
        let profile = workload
            .parent()
            .ok_or_else(|| io::Error::other("workload executable has no parent directory"))?;
        let target = profile
            .parent()
            .ok_or_else(|| io::Error::other("profile directory has no parent directory"))?;
        let name = profile
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("profile directory name is not UTF-8"))?;
        let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
            .args(["build", "--quiet", "-p", "meowland", "--bin", "meowland"])
            .arg("--target-dir")
            .arg(target);
        match name {
            "debug" => {}
            "release" => {
                command.arg("--release");
            }
            name => {
                command.args(["--profile", name]);
            }
        }
        let status = command.status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "building meowland failed: {status}"
            )));
        }
        Ok(Self {
            meowland: profile.join("meowland"),
            workload,
        })
    }
}
