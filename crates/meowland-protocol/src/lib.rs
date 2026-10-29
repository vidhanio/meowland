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
    Button, ClipboardOfferId, ClipboardRequestId, ControlRequest, ControlResponse, FrameAck, Hello,
    Input, KEY_INSERT, KEY_Q, KEY_V, KEY_W, MAX_INPUT_TEXT, MAX_MESSAGE, Modifiers, OfferIntent,
    PaneAction, PanePacket, PaneToServer, REQUEST_TIMEOUT, RETAINED_BUFFER, Scroll, ServerToPane,
    Shortcut, Show, Transition, WindowId, WindowInfo, WindowState, decode, recv, release_oversized,
    sanitize, sanitize_with_limit, send,
};
pub use shared::{FrameTransport, SharedFrame, SharedObject};
