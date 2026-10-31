//! Window identity.

/// Compositor-internal identity of a window, never exposed to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl From<u64> for WindowId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
