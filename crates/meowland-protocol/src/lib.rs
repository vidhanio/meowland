//! Shared control and pane messages, clipboard data, and socket discovery.

#[cfg(feature = "async")]
pub mod asynchronous;

pub mod clipboard;
mod frame;
mod paths;
mod protocol;
mod shared;

pub use frame::FrameBand;
pub use paths::{Paths, PathsError};
pub use protocol::{
    Button, ControlRequest, ControlResponse, Follow, Hello, Input, KEY_INSERT, KEY_V, KeyBinding,
    MAX_HELD_KEYS, MAX_INPUT_TEXT, MAX_MESSAGE, Modifiers, PaneAction, PanePacket, PaneToServer,
    REQUEST_TIMEOUT, RETAINED_BUFFER, Scroll, Selector, ServerToPane, Shortcut, Shortcuts, Target,
    Transition, WindowInfo, WindowState, recv, release_oversized, sanitize, sanitize_with_limit,
    send, valid_key_identity,
};
pub use shared::{SharedFrame, SharedObject};
