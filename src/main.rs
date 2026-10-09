mod cli;

use tracing_subscriber::{EnvFilter, prelude::*};

fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("meowland=info"));
    match std::env::args_os().nth(1).as_deref() {
        Some(command) if command == "server" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();
        }
        Some(command) if command == "run" || command == "attach" => {
            if let Ok(layer) = tracing_journald::layer() {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(layer)
                    .init();
            }
        }
        _ => {}
    }
}

fn main() {
    init_logging();
    if let Err(error) = cli::start() {
        eprintln!("meowland: {error}");
        std::process::exit(1);
    }
}
