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

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    #[test]
    fn error_chains_render_each_source_once() {
        let server = anyhow::Error::new(meowland_server::Error::IoContext {
            action: "binding the control socket".into(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        })
        .context("starting server");
        assert_eq!(
            format!("{server:#}"),
            "starting server: binding the control socket: permission denied"
        );

        let terminal = anyhow::Error::new(meowland_terminal::Error::Connect {
            path: PathBuf::from("/run/user/1000/meowland-pane.sock"),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        });
        assert_eq!(
            format!("{terminal:#}"),
            "could not reach the pane socket /run/user/1000/meowland-pane.sock (is the server running?): connection refused"
        );
    }
}
