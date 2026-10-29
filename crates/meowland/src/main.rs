//! Command-line entry point for the compositor server and terminal-pane client.

mod cli;
mod configuration;
mod logging;

use std::io::{self, Write};

fn main() {
    let command = cli::parse();
    logging::init(command.log_destination());
    if let Err(error) = cli::run(command) {
        let _ = writeln!(io::stderr().lock(), "meowland: {}", format_error(&error));
        std::process::exit(1);
    }
}

fn format_error(error: &anyhow::Error) -> String {
    let mut rendered = String::new();
    let mut skip_source = false;
    for (index, cause) in error.chain().enumerate() {
        if skip_source {
            skip_source = false;
            continue;
        }
        if index > 0 {
            rendered.push_str(": ");
        }
        rendered.push_str(&cause.to_string());
        skip_source = source_is_in_display(cause);
    }
    rendered
}

fn source_is_in_display(cause: &(dyn std::error::Error + 'static)) -> bool {
    cause
        .downcast_ref::<meowland_server::Error>()
        .is_some_and(meowland_server::Error::source_is_in_display)
        || cause
            .downcast_ref::<meowland_terminal::Error>()
            .is_some_and(meowland_terminal::Error::source_is_in_display)
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use super::format_error;

    #[test]
    fn error_chains_render_each_source_once() {
        let server = anyhow::Error::new(meowland_server::Error::IoContext {
            action: "binding the control socket".into(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        })
        .context("starting server");
        assert_eq!(
            format_error(&server),
            "starting server: binding the control socket: permission denied"
        );

        let terminal = anyhow::Error::new(meowland_terminal::Error::Connect {
            path: PathBuf::from("/run/user/1000/meowland-pane.sock"),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        });
        assert_eq!(
            format_error(&terminal),
            "could not reach the pane socket /run/user/1000/meowland-pane.sock (is the server running?): connection refused"
        );
    }

    #[test]
    fn coincidental_source_suffixes_are_still_rendered() {
        let source = io::Error::from_raw_os_error(2).to_string();
        let path = format!("/tmp/{source}");
        let error = anyhow::Error::new(io::Error::from_raw_os_error(2))
            .context(format!("reading configuration {path}"));

        assert_eq!(
            format_error(&error),
            format!("reading configuration {path}: {source}")
        );
    }
}
