//! Library-facing error formatting.

use std::{io, path::PathBuf};

use meowland_terminal::Error;

#[test]
fn connect_display_includes_the_socket_and_source() {
    let error = Error::Connect {
        path: PathBuf::from("/run/user/1000/meowland-pane.sock"),
        source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
    };

    assert_eq!(
        error.to_string(),
        "could not reach the pane socket /run/user/1000/meowland-pane.sock (is the server running?): connection refused"
    );
    assert!(std::error::Error::source(&error).is_some());
}
