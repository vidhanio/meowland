//! Machine-readable failures from `meowland --json`.

use std::process::{Command, Stdio};

#[test]
fn json_failures_are_one_error_object_on_stderr() {
    let runtime = tempfile::tempdir().expect("create runtime directory");
    let output = Command::new(env!("CARGO_BIN_EXE_meowland"))
        .args(["--json", "list"])
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdin(Stdio::null())
        .output()
        .expect("run meowland list");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr is one JSON document");
    let message = error["error"].as_str().expect("error is a string");
    assert!(
        message.starts_with("cannot reach the server"),
        "message: {message}"
    );
}
