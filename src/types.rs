//! Strongly typed identifiers and protocol scalars.

use nutype::nutype;

/// The stable ID assigned to a Wayland toplevel by the compositor.
#[nutype(
    const_fn,
    derive(
        Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display, FromStr
    )
)]
pub struct WindowId(u64);

/// The ID assigned to one terminal pane connection.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct PaneId(u64);

/// The version of the pane protocol spoken on a connection.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Display))]
pub struct ProtocolVersion(u32);
/// An image ID in the kitty graphics protocol.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct ImageId(u32);

/// A process ID used while walking and stopping the server's process tree.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct ProcessId(u32);

/// A Linux device number associated with a render node.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct DeviceId(u64);

/// An X11 display number reserved for xwayland-satellite.
#[nutype(
    const_fn,
    derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display)
)]
pub struct XDisplayNumber(u32);
