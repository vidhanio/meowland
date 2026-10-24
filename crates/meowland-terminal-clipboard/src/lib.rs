//! Capability-selected terminal clipboard transports and deferred input
//! actions.

use std::{io, time::Instant};

use meowland_protocol::clipboard::{ClipboardData, Deadline, Selection};

mod kitty;
mod osc52;

#[derive(Debug)]
pub enum Action {
    Offer {
        selection: Selection,
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

#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Kitty,
    Osc52,
}

enum Transport {
    Kitty(Box<kitty::Clipboard>),
    Text(osc52::Clipboard),
}

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

    /// Preserve an explicit text read across the terminal's permission dialog.
    pub fn focus(&mut self, focused: bool) -> bool {
        match &mut self.transport {
            Transport::Kitty(clipboard) => {
                clipboard.focus(focused);
                false
            }
            Transport::Text(clipboard) => clipboard.pending(),
        }
    }

    pub fn refresh(&mut self, selection: Selection, shortcut: bool) -> Option<Instant> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.refresh(selection, shortcut),
            Transport::Text(clipboard) => clipboard.refresh(selection, shortcut),
        }
    }

    pub fn cancel_refresh(&mut self) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel_refresh(),
            Transport::Text(clipboard) => clipboard.cancel_refresh(),
        }
    }

    pub fn read(&mut self, request: u64, offer: u64, mime: String, deadline: Deadline) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.read(request, offer, mime, deadline),
            Transport::Text(clipboard) => clipboard.read(request, offer, &mime, deadline),
        }
    }

    pub fn cancel(&mut self, request: u64) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.cancel(request),
            Transport::Text(clipboard) => clipboard.cancel(request),
        }
    }

    pub fn copy(&mut self, selection: Selection, data: ClipboardData, output: &mut Vec<u8>) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.copy(selection, data),
            Transport::Text(_) => osc52::Clipboard::copy(selection, &data, output),
        }
    }

    pub fn next_action(&mut self) -> Option<Action> {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.next_action(),
            Transport::Text(clipboard) => clipboard.next_action(),
        }
    }

    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match &self.transport {
            Transport::Kitty(clipboard) => clipboard.deadline(),
            Transport::Text(clipboard) => clipboard.deadline(),
        }
    }

    pub fn advance(&mut self, output: &mut Vec<u8>) {
        match &mut self.transport {
            Transport::Kitty(clipboard) => clipboard.poll(output),
            Transport::Text(clipboard) => clipboard.poll(output),
        }
    }

    pub fn packet(&mut self, bytes: &[u8]) {
        if let Transport::Kitty(clipboard) = &mut self.transport {
            clipboard.packet(bytes);
        }
    }

    pub fn text(&mut self, bytes: &[u8]) {
        if let Transport::Text(clipboard) = &mut self.transport {
            clipboard.text(bytes);
        }
    }
}
