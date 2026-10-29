//! Capability-selected terminal clipboard transports and deferred input
//! actions.

use std::{io, time::Instant};

use meowland_protocol::clipboard::{ClipboardData, Deadline, Selection};

use crate::{kitty, osc52};

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
    /// Update MIME types without requesting legacy OSC 52 reads.
    Background,
    /// Refresh before forwarding paste, allowing a terminal permission dialog.
    Paste,
}

/// Terminal focus transition affecting clipboard notifications and permission
/// dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Resume background selection refreshes.
    Gained,
    /// Stop background refreshes without interrupting an explicit permission
    /// dialog.
    Lost,
}

/// Clipboard protocol advertised by the terminal during capability probing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// MIME-aware, permission-aware OSC 5522 transfers.
    Kitty,
    /// Legacy OSC 52 plain-text transfers.
    Osc52,
}

enum Transport {
    Kitty(Box<kitty::Clipboard>),
    Text(osc52::Clipboard),
}

/// Bounded terminal selection state with serialized requests and deferred
/// server actions.
pub struct Clipboard {
    transport: Transport,
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let protocol = match &self.transport {
            Transport::Kitty(_) => Protocol::Kitty,
            Transport::Text(_) => Protocol::Osc52,
        };
        f.debug_struct("Clipboard")
            .field("protocol", &protocol)
            .finish_non_exhaustive()
    }
}

impl Clipboard {
    /// Select the clipboard protocol advertised by the terminal.
    ///
    /// # Errors
    /// Returns an error if secure request credentials cannot be generated.
    pub fn new(protocol: Protocol) -> io::Result<Self> {
        let transport = match protocol {
            Protocol::Kitty => Transport::Kitty(Box::new(kitty::Clipboard::new()?)),
            Protocol::Osc52 => Transport::Text(osc52::Clipboard::default()),
        };
        Ok(Self { transport })
    }

    /// Apply focus and report whether deferred input must survive an active
    /// permission dialog.
    pub fn focus(&mut self, focus: Focus) -> bool {
        match &mut self.transport {
            Transport::Kitty(clipboard) => {
                clipboard.focus(focus);
                false
            }
            Transport::Text(clipboard) => clipboard.pending(),
        }
    }

    /// Request metadata, returning an input-deferral deadline when a paste
    /// needs it.
    pub fn refresh(&mut self, selection: Selection, refresh: Refresh) -> Option<Instant> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.refresh(selection, refresh),
            Transport::Text(clipboard) => clipboard.refresh(selection, refresh),
        }
    }

    /// Supersede queued refreshes for one selection after an explicit paste or
    /// copy.
    pub fn cancel_refresh_selection(&mut self, selection: Selection) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel_refresh_selection(selection),
            Transport::Text(clipboard) => {
                if selection == Selection::Clipboard {
                    clipboard.cancel_refresh();
                }
            }
        }
    }

    /// Queue a representation read, reporting unavailable or expired offers
    /// through [`Action::Reply`].
    pub fn read(&mut self, request: u64, offer: u64, mime: String, deadline: Deadline) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.read(request, offer, mime, deadline),
            Transport::Text(clipboard) => clipboard.read(request, offer, &mime, deadline),
        }
    }

    /// Suppress a pending read and any queued reply with the given request
    /// identity.
    pub fn cancel(&mut self, request: u64) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel(request),
            Transport::Text(clipboard) => clipboard.cancel(request),
        }
    }

    /// Copy bounded data, appending immediate OSC 52 commands or queuing a
    /// Kitty transfer.
    pub fn copy(&mut self, selection: Selection, data: ClipboardData, output: &mut Vec<u8>) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.copy(selection, data),
            Transport::Text(_) => osc52::Clipboard::copy(selection, &data, output),
        }
    }

    /// Pop the next server or input action in transport order.
    pub fn next_action(&mut self) -> Option<Action> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.next_action(),
            Transport::Text(clipboard) => clipboard.next_action(),
        }
    }

    /// Earliest active or queued operation deadline that should wake the
    /// session.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match &self.transport {
            Transport::Kitty(clipboard) => clipboard.deadline(),
            Transport::Text(clipboard) => clipboard.deadline(),
        }
    }

    /// Expire requests and append the next transfer when terminal output is
    /// available.
    pub fn advance(&mut self, output: &mut Vec<u8>) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.advance(output),
            Transport::Text(clipboard) => clipboard.poll(output),
        }
    }

    /// Accept an OSC 5522 reply or notification; ignored for the legacy
    /// transport.
    pub fn packet(&mut self, bytes: &[u8]) {
        if let Transport::Kitty(clipboard) = &mut self.transport {
            clipboard.packet(bytes);
        }
    }

    /// Accept a base64 OSC 52 reply; ignored for the Kitty transport.
    pub fn text(&mut self, bytes: &[u8]) {
        if let Transport::Text(clipboard) = &mut self.transport {
            clipboard.text(bytes);
        }
    }
}
