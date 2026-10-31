//! Window identity.

/// Compositor-internal identity of a window, never exposed to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    /// Numeric identity in creation order.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for WindowId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
