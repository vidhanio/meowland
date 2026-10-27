//! Shared control and pane messages, clipboard data, and socket discovery.

#[cfg(feature = "async")]
pub mod asynchronous;

pub mod clipboard;
mod frame;
mod paths;
mod protocol;
mod shared;

pub use frame::FrameBand;
pub use paths::Paths;
pub use protocol::{
    ControlRequest, ControlResponse, FrameAck, Hello, Input, KEY_INSERT, KEY_Q, KEY_V, KEY_W,
    MAX_INPUT_TEXT, MAX_MESSAGE, PaneAction, PanePacket, PaneToServer, REQUEST_TIMEOUT,
    ServerToPane, Shortcut, Show, WindowInfo, decode, modifiers, recv, sanitize,
    sanitize_with_limit, send,
};
pub use shared::{FrameTransport, SharedFrame, SharedObject};
