mod cli;

use tracing_subscriber::{EnvFilter, prelude::*};

#[derive(Clone, Copy)]
enum LogDestination {
    Stderr,
    Journal,
    None,
}

fn init_logging(destination: LogDestination) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("meowland=info"));
    match destination {
        LogDestination::Stderr => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();
        }
        LogDestination::Journal => {
            if let Ok(layer) = tracing_journald::layer() {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(layer)
                    .init();
            }
        }
        LogDestination::None => {}
    }
}

fn main() {
    if let Err(error) = cli::start() {
        eprintln!("meowland: {error}");
        std::process::exit(1);
    }
}
