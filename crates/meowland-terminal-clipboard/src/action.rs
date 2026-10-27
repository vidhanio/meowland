//! Clipboard selection actions and terminal focus transitions.

use meowland_protocol::clipboard::Selection;

/// Deferred clipboard work to forward to the server or apply to queued input.
#[derive(Debug)]
pub enum Action {
    /// Publish a terminal selection and optionally trigger application paste.
    Offer {
        /// Selection whose contents changed.
        selection: Selection,
        /// Terminal-assigned identity used for subsequent reads.
        offer: u64,
        /// Validated MIME representations available from this offer.
        mimes: Vec<String>,
        /// Whether this originated from an explicit paste notification.
        paste: bool,
    },
    /// Complete a server-requested selection read.
    Reply {
        /// Server-assigned read identity.
        request: u64,
        /// Raw representation bytes, or `None` if unavailable.
        data: Option<Vec<u8>>,
    },
    /// Resume input deferred while selection metadata was being refreshed.
    Unblock,
}

/// Why selection metadata is being requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Refresh the available MIME types without triggering application paste.
    Background,
    /// Refresh before forwarding paste, allowing a terminal permission dialog.
    Paste,
}

/// Terminal focus transition affecting clipboard notifications and permission dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Resume background selection refreshes.
    Gained,
    /// Stop background refreshes without cancelling explicit reads.
    Lost,
}
