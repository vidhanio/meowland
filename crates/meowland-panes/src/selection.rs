//! Window identity and the selectors panes wait on.

use meowland_protocol::Selector;

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

/// How a pane identifies the windows it may show.
#[derive(Clone, Debug)]
pub enum Selection {
    /// Match windows with this selector.
    Matching(Selector),
    /// Match the window claimed by this activation token.
    Activation(String),
}
