//! Plain-text clipboard limits.

/// Maximum text bytes carried by a bracketed paste or an OSC 52 copy.
pub const MAX_TEXT: usize = 1024 * 1024;
