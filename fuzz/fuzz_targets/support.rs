//! Invariants shared by the terminal input fuzz targets.

use meowland_protocol::MAX_INPUT_TEXT;

/// Committed text must be nonempty, bounded, and free of control characters.
pub fn assert_committed(text: &str) {
    assert!(!text.is_empty(), "empty committed text");
    assert!(text.len() <= MAX_INPUT_TEXT, "oversized committed text");
    assert!(
        !text.chars().any(char::is_control),
        "control character in committed text: {text:?}"
    );
}
