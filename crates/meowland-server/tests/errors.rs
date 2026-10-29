//! Library-facing error formatting.

use std::io;

use meowland_server::Error;

#[test]
fn io_context_display_includes_the_action_and_source() {
    let error = Error::IoContext {
        action: "binding the control socket".into(),
        source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
    };

    assert_eq!(
        error.to_string(),
        "binding the control socket: permission denied"
    );
    assert!(std::error::Error::source(&error).is_some());
}
