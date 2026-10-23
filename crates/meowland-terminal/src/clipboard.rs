//! Capability-selected terminal clipboard transports and deferred input
//! actions.

use std::{io, time::Instant};

use meowland_protocol::clipboard::{ClipboardData, Deadline};

use super::output::Output;

mod kitty;
mod osc52;

pub enum Action {
    Offer {
        offer: u64,
        mimes: Vec<String>,
        paste: bool,
    },
    Reply {
        request: u64,
        data: Option<Vec<u8>>,
    },
    Unblock,
}

enum Transport {
    Kitty(Box<kitty::Clipboard>),
    Text(osc52::Clipboard),
}

pub struct Clipboard {
    transport: Transport,
}

impl Clipboard {
    pub(super) fn new(kitty: bool) -> io::Result<Self> {
        let transport = if kitty {
            Transport::Kitty(Box::new(kitty::Clipboard::new()?))
        } else {
            Transport::Text(osc52::Clipboard::default())
        };
        Ok(Self { transport })
    }

    /// Preserve an explicit text read across the terminal's permission dialog.
    pub(super) fn focus(&mut self, focused: bool) -> bool {
        match &mut self.transport {
            Transport::Kitty(clipboard) => {
                clipboard.focus(focused);
                false
            }
            Transport::Text(clipboard) => clipboard.pending(),
        }
    }

    pub(super) fn refresh(&mut self, shortcut: bool) -> Option<Instant> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.refresh(shortcut),
            Transport::Text(clipboard) => clipboard.refresh(shortcut),
        }
    }

    pub(super) fn cancel_refresh(&mut self) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel_refresh(),
            Transport::Text(clipboard) => clipboard.cancel_refresh(),
        }
    }

    pub(super) fn read(&mut self, request: u64, offer: u64, mime: String, deadline: Deadline) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.read(request, offer, mime, deadline),
            Transport::Text(clipboard) => clipboard.read(request, offer, &mime, deadline),
        }
    }

    pub(super) fn cancel(&mut self, request: u64) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel(request),
            Transport::Text(clipboard) => clipboard.cancel(request),
        }
    }

    pub(super) fn copy(&mut self, data: ClipboardData, output: &mut Output) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.copy(data),
            Transport::Text(_) => osc52::Clipboard::copy(&data, output),
        }
    }

    pub(super) fn next_action(&mut self) -> Option<Action> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.next_action(),
            Transport::Text(clipboard) => clipboard.next_action(),
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match &self.transport {
            Transport::Kitty(clipboard) => clipboard.deadline(),
            Transport::Text(clipboard) => clipboard.deadline(),
        }
    }

    pub(super) fn poll(&mut self, output: &mut Output) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.poll(output),
            Transport::Text(clipboard) => clipboard.poll(output),
        }
    }

    pub(super) fn packet(&mut self, bytes: &[u8]) {
        if let Transport::Kitty(clipboard) = &mut self.transport {
            clipboard.packet(bytes);
        }
    }

    pub(super) fn text(&mut self, bytes: &[u8]) {
        if let Transport::Text(clipboard) = &mut self.transport {
            clipboard.text(bytes);
        }
    }
}
