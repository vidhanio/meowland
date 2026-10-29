//! On-demand OSC 52 text reads with bounded snapshots and uncorrelated-reply
//! draining.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use meowland_protocol::{
    ClipboardOfferId, ClipboardRequestId,
    clipboard::{ClipboardData, Deadline, MAX_TEXT, Selection, TEXT_MIMES},
};

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
    snapshot: Option<(ClipboardOfferId, Vec<u8>)>,
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

    pub(crate) fn read(
        &mut self,
        request: ClipboardRequestId,
        offer: ClipboardOfferId,
        mime: &str,
        deadline: Deadline,
    ) {
        let data = self.snapshot.as_ref().and_then(|(id, bytes)| {
            (*id == offer && TEXT_MIMES.contains(&mime) && !deadline.expired())
                .then(|| bytes.clone())
        });
        self.actions.push_back(Action::Reply { request, data });
    }

    pub(crate) fn cancel(&mut self, request: ClipboardRequestId) {
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
            let offer = ClipboardOfferId::from(self.next);
            self.snapshot = Some((offer, bytes));
            self.actions.push_back(Action::Offer {
                selection: Selection::Clipboard,
                offer,
                mimes: TEXT_MIMES.into_iter().map(str::to_owned).collect(),
                paste: false,
            });
        } else {
            tracing::warn!("Terminal clipboard read returned invalid text");
        }
        self.actions.push_back(Action::Unblock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading() -> Clipboard {
        let mut clipboard = Clipboard::default();
        let deadline = clipboard.refresh(Selection::Clipboard, Refresh::Paste);
        assert!(deadline.is_some());
        let mut output = Vec::new();
        clipboard.poll(&mut output);
        assert_eq!(output, b"\x1b]52;c;?\x1b\\");
        clipboard
    }

    fn expect_offer(clipboard: &mut Clipboard) -> ClipboardOfferId {
        match clipboard.next_action() {
            Some(Action::Offer {
                selection,
                offer,
                mimes,
                paste,
            }) => {
                assert_eq!(selection, Selection::Clipboard);
                assert_eq!(
                    mimes,
                    TEXT_MIMES
                        .iter()
                        .map(|mime| (*mime).to_owned())
                        .collect::<Vec<_>>()
                );
                assert!(!paste);
                offer
            }
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    fn expect_unblock(clipboard: &mut Clipboard) {
        assert!(matches!(clipboard.next_action(), Some(Action::Unblock)));
        assert!(clipboard.next_action().is_none());
    }

    #[test]
    fn text_accepts_valid_utf8() {
        let mut clipboard = reading();
        clipboard.text(STANDARD.encode("hello").as_bytes());
        let _offer = expect_offer(&mut clipboard);
        expect_unblock(&mut clipboard);
    }

    #[test]
    fn text_accepts_exactly_max_text() {
        let payload = vec![b'a'; MAX_TEXT];
        let mut clipboard = reading();
        clipboard.text(STANDARD.encode(&payload).as_bytes());
        let _offer = expect_offer(&mut clipboard);
        expect_unblock(&mut clipboard);
    }

    #[test]
    fn text_rejects_decoded_payloads_over_max_text() {
        let payload = vec![b'a'; MAX_TEXT + 1];
        let mut clipboard = reading();
        clipboard.text(STANDARD.encode(&payload).as_bytes());
        assert!(matches!(clipboard.next_action(), Some(Action::Unblock)));
        assert!(clipboard.next_action().is_none());
    }

    #[test]
    fn text_rejects_invalid_base64_and_utf8() {
        for payload in [STANDARD.encode([0xff]), "%%%".to_owned()] {
            let mut clipboard = reading();
            clipboard.text(payload.as_bytes());
            assert!(matches!(clipboard.next_action(), Some(Action::Unblock)));
            assert!(clipboard.next_action().is_none());
        }
    }

    #[test]
    fn text_is_ignored_when_idle() {
        let mut clipboard = Clipboard::default();
        clipboard.text(STANDARD.encode("hello").as_bytes());
        assert!(clipboard.next_action().is_none());
    }

    #[test]
    fn text_drains_an_uncorrelated_reply() {
        let mut clipboard = reading();
        clipboard.cancel_refresh();
        clipboard.text(STANDARD.encode("hello").as_bytes());
        assert!(clipboard.next_action().is_none());
    }

    #[test]
    fn read_returns_the_snapshot_for_text_mimes() {
        let mut clipboard = reading();
        clipboard.text(STANDARD.encode("hello").as_bytes());
        let offer = expect_offer(&mut clipboard);
        expect_unblock(&mut clipboard);

        let request = ClipboardRequestId::from(9);
        clipboard.read(
            request,
            offer,
            "text/plain",
            Deadline::new(Duration::from_secs(1)),
        );
        match clipboard.next_action() {
            Some(Action::Reply {
                request: got,
                data: Some(data),
            }) => {
                assert_eq!(got, request);
                assert_eq!(data, b"hello");
            }
            other => panic!("expected a reply, got {other:?}"),
        }
        assert!(clipboard.next_action().is_none());
    }
}
