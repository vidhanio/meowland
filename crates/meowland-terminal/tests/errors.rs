//! Library-facing error formatting.

use std::{io, path::PathBuf};

use meowland_terminal::Error;

#[test]
fn connect_leaves_its_cause_to_the_source_chain() {
    let error = Error::Connect {
        path: PathBuf::from("/run/user/1000/meowland-pane.sock"),
        source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
    };

    assert!(!error.to_string().contains("connection refused"));
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some("connection refused".into())
    );
}
