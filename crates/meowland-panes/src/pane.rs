//! Pane identity.

/// Host-assigned identity of a terminal pane, distinct from a Wayland window
/// ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(u64);

impl PaneId {
    /// Numeric identity as assigned by the server.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for PaneId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
