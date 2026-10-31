//! Library-facing error formatting.

use std::io;

use meowland_server::Error;

#[test]
fn io_context_leaves_its_cause_to_the_source_chain() {
    let error = Error::IoContext {
        action: "binding the control socket".into(),
        source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
    };

    assert!(!error.to_string().contains("permission denied"));
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some("permission denied".into())
    );
}
