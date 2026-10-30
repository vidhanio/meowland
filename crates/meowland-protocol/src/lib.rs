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
    Button, ClipboardOfferId, ClipboardRequestId, ControlRequest, ControlResponse, Follow,
    FrameAck, Hello, Input, KEY_INSERT, KEY_V, KeyBinding, MAX_INPUT_TEXT, MAX_MESSAGE, Modifiers,
    OfferIntent, PaneAction, PanePacket, PaneToServer, REQUEST_TIMEOUT, RETAINED_BUFFER, Scroll,
    Selector, ServerToPane, Shortcut, Shortcuts, Target, Transition, WindowInfo, WindowState,
    decode, recv, release_oversized, sanitize, sanitize_with_limit, send,
};
pub use shared::{FrameTransport, SharedFrame, SharedObject};
