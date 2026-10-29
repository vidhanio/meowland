//! Tracing setup for the server process and the terminal-pane client.

use std::{fs, io, path::PathBuf};

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Where tracing events for one command should go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    /// Server diagnostics on standard error, as seen by systemd or a shell.
    Stderr,
    /// Pane-client diagnostics in the journal, or in a state file while the
    /// terminal is in raw mode.
    Journal,
    /// The command produces only its own output.
    None,
}

/// Install the process-wide tracing subscriber for `destination`.
pub(crate) fn init(destination: Destination) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("meowland=info"));
    match destination {
        Destination::Stderr => {
            fmt()
                .with_env_filter(filter)
                .with_writer(io::stderr)
                .with_ansi(false)
                .init();
        }
        Destination::Journal => init_journal(filter),
        Destination::None => {}
    }
}

fn init_journal(filter: EnvFilter) {
    if let Ok(layer) = tracing_journald::layer() {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .init();
        return;
    }
    if let Ok(file) = pane_log() {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_writer(file).with_ansi(false))
            .init();
    }
}

/// Stderr is unusable for a pane client because it owns the terminal, so
/// journald failures fall back to an append-only state file.
fn pane_log() -> io::Result<fs::File> {
    let directory = dirs::state_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot locate state directory"))?
        .join("meowland");
    fs::create_dir_all(&directory)?;
    let path: PathBuf = directory.join("pane.log");
    fs::OpenOptions::new().create(true).append(true).open(path)
}
