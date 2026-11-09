//! Capability-selected terminal clipboard transports and deferred input
//! actions.

use std::{fmt, io, time::Instant};

use meowland_protocol::{
    ClipboardOfferId, ClipboardRequestId, OfferIntent,
    clipboard::{ClipboardData, Deadline, Selection, Transport},
};

use crate::{kitty, osc52};

/// Deferred clipboard work to forward to the server or apply to queued input.
#[derive(Debug)]
pub enum Action {
    /// Publish a terminal selection and optionally trigger application paste.
    Offer {
        /// Selection whose contents changed.
        selection: Selection,
        /// Terminal-assigned identity used for subsequent reads.
        offer: ClipboardOfferId,
        /// Validated MIME representations available from this offer.
        mimes: Vec<String>,
        /// [`OfferIntent::Paste`] when the terminal reported an explicit paste.
        intent: OfferIntent,
    },
    /// Complete a server-requested selection read.
    Reply {
        /// Server-assigned read identity.
        request: ClipboardRequestId,
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
pub(crate) enum Focus {
    /// Resume background selection refreshes.
    Gained,
    /// Stop background refreshes without interrupting an explicit permission
    /// dialog.
    Lost,
}

/// What a focus loss does to input deferred behind a clipboard refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredInput {
    /// No refresh can complete any more; drop the deferred input.
    Discard,
    /// A permission dialog may have taken focus; keep the input until the
    /// refresh completes.
    Keep,
}

enum Backend {
    Kitty(Box<kitty::Clipboard>),
    Text(osc52::Clipboard),
}

/// Bounded terminal selection state with serialized requests and deferred
/// server actions.
pub struct Clipboard {
    backend: Backend,
}

impl fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clipboard")
            .field("transport", &self.transport())
            .finish_non_exhaustive()
    }
}

impl Clipboard {
    /// Select the clipboard transport advertised by the terminal.
    ///
    /// # Errors
    /// Returns an error if secure request credentials cannot be generated.
    pub fn new(transport: Transport) -> io::Result<Self> {
        let backend = match transport {
            Transport::Kitty => Backend::Kitty(Box::new(kitty::Clipboard::new()?)),
            Transport::Osc52 => Backend::Text(osc52::Clipboard::default()),
        };
        Ok(Self { backend })
    }

    /// The transport this clipboard speaks.
    #[must_use]
    pub const fn transport(&self) -> Transport {
        match self.backend {
            Backend::Kitty(_) => Transport::Kitty,
            Backend::Text(_) => Transport::Osc52,
        }
    }

    /// Resume background selection refreshes after the terminal regains focus.
    pub fn focus_gained(&mut self) {
        if let Backend::Kitty(clipboard) = &mut self.backend {
            clipboard.focus(Focus::Gained, Instant::now());
        }
    }

    /// Stop background refreshes and report what happens to deferred input.
    pub fn focus_lost(&mut self) -> DeferredInput {
        match &mut self.backend {
            Backend::Kitty(clipboard) => {
                clipboard.focus(Focus::Lost, Instant::now());
                DeferredInput::Discard
            }
            Backend::Text(clipboard) if clipboard.pending() => DeferredInput::Keep,
            Backend::Text(_) => DeferredInput::Discard,
        }
    }

    /// Request metadata, returning an input-deferral deadline when a paste
    /// needs it.
    pub fn refresh(&mut self, selection: Selection, refresh: Refresh) -> Option<Instant> {
        let now = Instant::now();
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.refresh(selection, refresh, now),
            Backend::Text(clipboard) => clipboard.refresh(selection, refresh, now),
        }
    }

    /// Supersede queued refreshes for one selection after an explicit paste or
    /// copy.
    pub fn cancel_refresh_selection(&mut self, selection: Selection) {
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.cancel_refresh_selection(selection),
            Backend::Text(clipboard) => {
                if selection == Selection::Clipboard {
                    clipboard.cancel_refresh(Instant::now());
                }
            }
        }
    }

    /// Queue a representation read, reporting unavailable or expired offers
    /// through [`Action::Reply`].
    pub fn read(
        &mut self,
        request: ClipboardRequestId,
        offer: ClipboardOfferId,
        mime: String,
        deadline: Deadline,
    ) {
        match &mut self.backend {
            Backend::Kitty(clipboard) => {
                clipboard.read(request, offer, mime, deadline, Instant::now());
            }
            Backend::Text(clipboard) => clipboard.read(request, offer, &mime, deadline),
        }
    }

    /// Suppress a pending read and any queued reply with the given request
    /// identity.
    pub fn cancel(&mut self, request: ClipboardRequestId) {
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.cancel(request),
            Backend::Text(clipboard) => clipboard.cancel(request),
        }
    }

    /// Copy bounded data, appending immediate OSC 52 commands or queuing a
    /// Kitty transfer.
    pub fn copy(&mut self, selection: Selection, data: ClipboardData, output: &mut Vec<u8>) {
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.copy(selection, data, Instant::now()),
            Backend::Text(_) => osc52::Clipboard::copy(selection, &data, output),
        }
    }

    /// Pop the next server or input action in transport order.
    pub fn next_action(&mut self) -> Option<Action> {
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.next_action(),
            Backend::Text(clipboard) => clipboard.next_action(),
        }
    }

    /// Earliest active or queued operation deadline that should wake the
    /// session.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match &self.backend {
            Backend::Kitty(clipboard) => clipboard.deadline(),
            Backend::Text(clipboard) => clipboard.deadline(),
        }
    }

    /// Expire requests and append the next transfer when terminal output is
    /// available.
    pub fn advance(&mut self, output: &mut Vec<u8>) {
        let now = Instant::now();
        match &mut self.backend {
            Backend::Kitty(clipboard) => clipboard.advance(now, output),
            Backend::Text(clipboard) => clipboard.poll(now, output),
        }
    }

    /// Accept an OSC 5522 reply or notification; ignored for the legacy
    /// transport.
    pub fn packet(&mut self, bytes: &[u8]) {
        if let Backend::Kitty(clipboard) = &mut self.backend {
            clipboard.packet(bytes, Instant::now());
        }
    }

    /// Accept a base64 OSC 52 reply; ignored for the Kitty transport.
    pub fn text(&mut self, bytes: &[u8]) {
        if let Backend::Text(clipboard) = &mut self.backend {
            clipboard.text(bytes, Instant::now());
        }
    }
}
