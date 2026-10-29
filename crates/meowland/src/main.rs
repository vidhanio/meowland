//! Command-line entry point for the compositor server and terminal-pane client.

mod cli;
mod configuration;
mod logging;

use std::io::{self, Write};

fn main() {
    let command = cli::parse();
    logging::init(command.log_destination());
    if let Err(error) = cli::run(command) {
        let _ = writeln!(io::stderr().lock(), "meowland: {error:#}");
        std::process::exit(1);
    }
}
