//! On-demand OSC 52 text reads with bounded snapshots and uncorrelated-reply
//! draining.

use std::{
    collections::VecDeque,
    io::Write as _,
    time::{Duration, Instant},
};

use base64::{Engine, display::Base64Display, engine::general_purpose::STANDARD};
use meowland_protocol::{
    ClipboardOfferId, ClipboardRequestId, OfferIntent,
    clipboard::{ClipboardData, Deadline, MAX_TEXT, Selection, TEXT_MIMES},
};

use super::{Action, Refresh};

const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a timed-out query's late reply is awaited before new reads resume.
///
/// OSC 52 replies carry no identity, so a reply arriving after this window
/// is indistinguishable from the reply to the next query.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Idle,
    Queued {
        deadline: Instant,
    },
    Reading {
        deadline: Instant,
    },
    /// No new query may start until the uncorrelated old reply is discarded
    /// or `until` passes.
    Draining {
        until: Instant,
    },
}

impl State {
    const fn deadline(self) -> Option<Instant> {
        match self {
            Self::Queued { deadline } | Self::Reading { deadline } => Some(deadline),
            Self::Draining { until } => Some(until),
            Self::Idle => None,
        }
    }
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
        matches!(self.state, State::Queued { .. } | State::Reading { .. })
    }

    pub(crate) fn cancel_refresh(&mut self, now: Instant) {
        self.state = match self.state {
            State::Queued { .. } => State::Idle,
            State::Reading { .. } => State::Draining {
                until: now + DRAIN_TIMEOUT,
            },
            state @ (State::Idle | State::Draining { .. }) => state,
        };
    }

    pub(crate) fn refresh(
        &mut self,
        selection: Selection,
        refresh: Refresh,
        now: Instant,
    ) -> Option<Instant> {
        self.expire(now);
        if selection == Selection::Primary
            || refresh != Refresh::Paste
            || !matches!(self.state, State::Idle)
        {
            return None;
        }
        let deadline = now + READ_TIMEOUT;
        self.state = State::Queued { deadline };
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
            write!(
                output,
                "\x1b]52;c;{}\x1b\\",
                Base64Display::new(text, &STANDARD)
            )
            .expect("writing to a Vec cannot fail");
        } else if data.items.is_empty() {
            output.extend_from_slice(b"\x1b]52;c;\x1b\\");
        }
    }

    pub(crate) fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub(crate) const fn deadline(&self) -> Option<Instant> {
        self.state.deadline()
    }

    fn expire(&mut self, now: Instant) {
        self.state = match self.state {
            State::Queued { deadline } if now >= deadline => {
                tracing::warn!("Terminal clipboard read expired before it could be sent");
                self.actions.push_back(Action::Unblock);
                State::Idle
            }
            State::Reading { deadline } if now >= deadline => {
                tracing::warn!(
                    "Terminal clipboard read timed out; suspending OSC 52 reads until its reply is drained"
                );
                self.actions.push_back(Action::Unblock);
                State::Draining {
                    until: now + DRAIN_TIMEOUT,
                }
            }
            State::Draining { until } if now >= until => State::Idle,
            state => state,
        };
    }

    pub(crate) fn poll(&mut self, now: Instant, output: &mut Vec<u8>) {
        self.expire(now);
        if let State::Queued { deadline } = self.state
            && output.is_empty()
        {
            output.extend_from_slice(b"\x1b]52;c;?\x1b\\");
            self.state = State::Reading { deadline };
        }
    }

    pub(crate) fn text(&mut self, payload: &[u8], now: Instant) {
        self.expire(now);
        match self.state {
            State::Draining { .. } => {
                self.state = State::Idle;
                return;
            }
            State::Reading { .. } => self.state = State::Idle,
            State::Idle | State::Queued { .. } => return,
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
                intent: OfferIntent::Refresh,
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

    fn reading(now: Instant) -> Clipboard {
        let mut clipboard = Clipboard::default();
        let deadline = clipboard.refresh(Selection::Clipboard, Refresh::Paste, now);
        assert_eq!(deadline, Some(now + READ_TIMEOUT));
        let mut output = Vec::new();
        clipboard.poll(now, &mut output);
        assert_eq!(output, b"\x1b]52;c;?\x1b\\");
        clipboard
    }

    fn expect_offer(clipboard: &mut Clipboard) -> ClipboardOfferId {
        match clipboard.next_action() {
            Some(Action::Offer {
                selection,
                offer,
                mimes,
                intent,
            }) => {
                assert_eq!(selection, Selection::Clipboard);
                assert_eq!(
                    mimes,
                    TEXT_MIMES
                        .iter()
                        .map(|mime| (*mime).to_owned())
                        .collect::<Vec<_>>()
                );
                assert_eq!(intent, OfferIntent::Refresh);
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
        let now = Instant::now();
        let mut clipboard = reading(now);
        clipboard.text(STANDARD.encode("hello").as_bytes(), now);
        let _offer = expect_offer(&mut clipboard);
        expect_unblock(&mut clipboard);
    }

    #[test]
    fn text_accepts_exactly_max_text() {
        let now = Instant::now();
        let payload = vec![b'a'; MAX_TEXT];
        let mut clipboard = reading(now);
        clipboard.text(STANDARD.encode(&payload).as_bytes(), now);
        let _offer = expect_offer(&mut clipboard);
        expect_unblock(&mut clipboard);
    }

    #[test]
    fn text_rejects_decoded_payloads_over_max_text() {
        let now = Instant::now();
        let payload = vec![b'a'; MAX_TEXT + 1];
        let mut clipboard = reading(now);
        clipboard.text(STANDARD.encode(&payload).as_bytes(), now);
        expect_unblock(&mut clipboard);
    }

    #[test]
    fn text_rejects_invalid_base64_and_utf8() {
        let now = Instant::now();
        for payload in [STANDARD.encode([0xff]), "%%%".to_owned()] {
            let mut clipboard = reading(now);
            clipboard.text(payload.as_bytes(), now);
            expect_unblock(&mut clipboard);
        }
    }

    #[test]
    fn text_is_ignored_when_idle() {
        let mut clipboard = Clipboard::default();
        clipboard.text(STANDARD.encode("hello").as_bytes(), Instant::now());
        assert!(clipboard.next_action().is_none());
    }

    #[test]
    fn text_drains_an_uncorrelated_reply() {
        let now = Instant::now();
        let mut clipboard = reading(now);
        clipboard.cancel_refresh(now);
        clipboard.text(STANDARD.encode("hello").as_bytes(), now);
        assert!(clipboard.next_action().is_none());
        assert_eq!(
            clipboard.refresh(Selection::Clipboard, Refresh::Paste, now),
            Some(now + READ_TIMEOUT)
        );
    }

    #[test]
    fn reads_resume_after_a_lost_reply_is_no_longer_awaited() {
        let now = Instant::now();
        let mut clipboard = reading(now);
        let timed_out = now + READ_TIMEOUT;
        let mut output = Vec::new();
        clipboard.poll(timed_out, &mut output);
        expect_unblock(&mut clipboard);
        assert_eq!(clipboard.deadline(), Some(timed_out + DRAIN_TIMEOUT));
        assert_eq!(
            clipboard.refresh(Selection::Clipboard, Refresh::Paste, timed_out),
            None
        );

        let drained = timed_out + DRAIN_TIMEOUT;
        assert_eq!(
            clipboard.refresh(Selection::Clipboard, Refresh::Paste, drained),
            Some(drained + READ_TIMEOUT)
        );
        clipboard.poll(drained, &mut output);
        assert_eq!(output, b"\x1b]52;c;?\x1b\\");
    }

    #[test]
    fn read_returns_the_snapshot_for_text_mimes() {
        let now = Instant::now();
        let mut clipboard = reading(now);
        clipboard.text(STANDARD.encode("hello").as_bytes(), now);
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
