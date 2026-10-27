//! On-demand OSC 52 text reads with bounded snapshots and uncorrelated-reply
//! draining.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use meowland_protocol::clipboard::{ClipboardData, Deadline, MAX_TEXT, Selection, TEXT_MIMES};

use super::{Action, Refresh};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Queued(Instant),
    Reading(Instant),
    /// No new query may start until the uncorrelated old reply is discarded.
    Draining,
}

#[derive(Default)]
pub(crate) struct Clipboard {
    state: State,
    next: u64,
    snapshot: Option<(u64, Vec<u8>)>,
    actions: VecDeque<Action>,
}

impl Clipboard {
    pub(crate) const fn pending(&self) -> bool {
        matches!(self.state, State::Queued(_) | State::Reading(_))
    }

    pub(crate) const fn cancel_refresh(&mut self) {
        self.state = match self.state {
            State::Queued(_) => State::Idle,
            State::Reading(_) => State::Draining,
            _ => return,
        };
    }

    pub(crate) fn refresh(&mut self, selection: Selection, refresh: Refresh) -> Option<Instant> {
        if selection == Selection::Primary
            || refresh != Refresh::Paste
            || !matches!(self.state, State::Idle)
        {
            return None;
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        self.state = State::Queued(deadline);
        Some(deadline)
    }

    pub(crate) fn read(&mut self, request: u64, offer: u64, mime: &str, deadline: Deadline) {
        let data = self.snapshot.as_ref().and_then(|(id, bytes)| {
            (*id == offer && TEXT_MIMES.contains(&mime) && !deadline.expired())
                .then(|| bytes.clone())
        });
        self.actions.push_back(Action::Reply { request, data });
    }

    pub(crate) fn cancel(&mut self, request: u64) {
        self.actions.retain(
            |action| !matches!(action, Action::Reply { request: number, .. } if *number == request),
        );
    }

    pub(crate) fn copy(selection: Selection, data: &ClipboardData, output: &mut Vec<u8>) {
        if selection == Selection::Primary || !data.valid() {
            return;
        }
        if let Some(text) = data.utf8_text() {
            output
                .extend_from_slice(format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text)).as_bytes());
        } else if data.items.is_empty() {
            output.extend_from_slice(b"\x1b]52;c;\x1b\\");
        }
    }

    pub(crate) fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub(crate) const fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Queued(deadline) | State::Reading(deadline) => Some(deadline),
            State::Idle | State::Draining => None,
        }
    }

    fn expire(&mut self) {
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let queued = matches!(self.state, State::Queued(_));
            self.state = if queued { State::Idle } else { State::Draining };
            self.actions.push_back(Action::Unblock);
            if queued {
                tracing::warn!("Terminal clipboard read expired before it could be sent");
            } else {
                tracing::warn!(
                    "Terminal clipboard read timed out; suspending OSC 52 reads until its reply is drained"
                );
            }
        }
    }

    pub(crate) fn poll(&mut self, output: &mut Vec<u8>) {
        self.expire();
        if let State::Queued(deadline) = self.state
            && output.is_empty()
        {
            output.extend_from_slice(b"\x1b]52;c;?\x1b\\");
            self.state = State::Reading(deadline);
        }
    }

    pub(crate) fn text(&mut self, payload: &[u8]) {
        self.expire();
        match self.state {
            State::Draining => {
                self.state = State::Idle;
                return;
            }
            State::Reading(_) => self.state = State::Idle,
            _ => return,
        }
        let data = (payload.len() <= MAX_TEXT.div_ceil(3) * 4)
            .then(|| STANDARD.decode(payload).ok())
            .flatten()
            .filter(|bytes| bytes.len() <= MAX_TEXT && std::str::from_utf8(bytes).is_ok());
        if let Some(bytes) = data {
            self.next += 1;
            self.snapshot = Some((self.next, bytes));
            self.actions.push_back(Action::Offer {
                selection: Selection::Clipboard,
                offer: self.next,
                mimes: TEXT_MIMES.into_iter().map(str::to_owned).collect(),
                paste: false,
            });
        } else {
            tracing::warn!("Terminal clipboard read returned invalid text");
        }
        self.actions.push_back(Action::Unblock);
    }
}
