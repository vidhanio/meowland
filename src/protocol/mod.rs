//! What meowland's parts say to each other, and the names they say it about.
//!
//! [`pane`] is the protocol between the server and one terminal pane, spoken
//! over a unix socket. [`control`] is what the command line says to a running
//! server.

use nutype::nutype;

pub mod control;
pub mod pane;

/// The stable ID assigned to a Wayland toplevel by the compositor.
#[nutype(
    const_fn,
    derive(
        Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display, FromStr
    )
)]
pub struct WindowId(u64);

/// The ID assigned to one attached terminal pane.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct PaneId(u64);

/// The version of the pane protocol spoken on a connection.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Display))]
pub struct ProtocolVersion(u32);
