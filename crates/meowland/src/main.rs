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
    for (index, cause) in error.chain().enumerate() {
        let cause = cause.to_string();
        if index > 0 && rendered.ends_with(&cause) {
            continue;
        }
        if index > 0 {
            rendered.push_str(": ");
        }
        rendered.push_str(&cause);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::format_error;

    #[derive(Debug)]
    struct Inner;

    impl fmt::Display for Inner {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("permission denied")
        }
    }

    impl std::error::Error for Inner {}

    #[derive(Debug)]
    struct Library {
        source: Inner,
    }

    impl fmt::Display for Library {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "binding: {}", self.source)
        }
    }

    impl std::error::Error for Library {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn error_chains_render_each_source_once() {
        let contextual = anyhow::Error::new(Library { source: Inner }).context("starting server");
        assert_eq!(
            format_error(&contextual),
            "starting server: binding: permission denied"
        );

        let ordinary = anyhow::anyhow!("permission denied").context("binding");
        assert_eq!(format_error(&ordinary), "binding: permission denied");
    }
}
