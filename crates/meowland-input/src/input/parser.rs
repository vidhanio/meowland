//! Streaming terminal decoder with bounded recovery from oversized controls.

#[cfg(test)]
use std::{cell::Cell, rc::Rc};
use std::{
    collections::VecDeque,
    io, mem,
    time::{Duration, Instant},
};

use meowland_protocol::clipboard::MAX_TEXT;
use vtparse::{CsiParam, VTActor, VTParser};

use super::{
    csi::csi,
    event::Event,
    mouse::{MAX_MOUSE, MOUSE_PREFIX, mouse},
};

const ESC: u8 = 0x1b;
/// Bytes a control sequence may span before it is discarded; terminal
/// input carries no control sequence this parser keeps that comes close.
const MAX_CONTROL: usize = 4096;
/// Bytes an oversized control sequence or bracketed paste may discard after
/// its limit before it must have terminated.
const MAX_DISCARDED: usize = MAX_CONTROL * 4;
/// Time an oversized control sequence or bracketed paste has to terminate
/// once it exceeds its limit.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);
const PASTE_START: [CsiParam; 1] = [CsiParam::Integer(200)];
const PASTE_END: &[u8] = b"\x1b[201~";

/// Time source for recovery deadlines that tests can freeze.
#[derive(Clone, Debug, Default)]
pub(super) struct Clock {
    #[cfg(test)]
    frozen: Rc<Cell<Option<Instant>>>,
}

impl Clock {
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests store a frozen time")
    )]
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.frozen.get() {
            return now;
        }
        Instant::now()
    }

    #[cfg(test)]
    pub(super) fn freeze(&self, now: Instant) {
        self.frozen.set(Some(now));
    }
}

/// An oversized input element that did not terminate within the recovery
/// limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputError {
    /// A control sequence whose bytes were being discarded.
    Control,
    /// A bracketed paste whose text was being discarded.
    Paste,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let element = match self {
            Self::Control => "control sequence",
            Self::Paste => "bracketed paste",
        };
        write!(
            formatter,
            "terminal input contained an unterminated {element} that exceeded the recovery limit"
        )
    }
}

impl std::error::Error for InputError {}

impl From<InputError> for io::Error {
    fn from(error: InputError) -> Self {
        Self::new(io::ErrorKind::InvalidData, error)
    }
}

/// Discarding an oversized input element, which must terminate within
/// [`MAX_DISCARDED`] bytes and before its deadline.
#[derive(Clone, Copy, Debug)]
struct Recovery {
    deadline: Instant,
    discarded: usize,
}

impl Recovery {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + RECOVERY_TIMEOUT,
            discarded: 0,
        }
    }

    /// Count one more discarded byte, failing once the budget is exhausted.
    const fn discard(&mut self, error: InputError) -> Result<(), InputError> {
        self.discarded += 1;
        if self.discarded > MAX_DISCARDED {
            return Err(error);
        }
        Ok(())
    }

    /// Fail if the element is still unterminated at `now`.
    fn check(&self, now: Instant, error: InputError) -> Result<(), InputError> {
        if now >= self.deadline {
            return Err(error);
        }
        Ok(())
    }
}

/// An SGR mouse report being collected, starting with [`MOUSE_PREFIX`].
#[derive(Clone, Copy)]
struct Report {
    bytes: [u8; MAX_MOUSE],
    len: usize,
}

impl Report {
    fn new() -> Self {
        let mut bytes = [0; MAX_MOUSE];
        bytes[..MOUSE_PREFIX.len()].copy_from_slice(MOUSE_PREFIX);
        Self {
            bytes,
            len: MOUSE_PREFIX.len(),
        }
    }

    /// Append a byte, returning false if the report is already full.
    const fn push(&mut self, byte: u8) -> bool {
        if self.len == MAX_MOUSE {
            return false;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        true
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Bracketed paste between its start and end markers.
enum Paste {
    /// Text that may still fit in [`MAX_TEXT`], including any partial end
    /// marker.
    Text(Vec<u8>),
    /// Oversized text being discarded; `tail` keeps only enough trailing
    /// bytes to recognize an end marker split across reads.
    Oversized { tail: Vec<u8>, recovery: Recovery },
}

/// What the next input byte means.
#[derive(Default)]
enum Mode {
    /// Bytes go to the VT parser.
    #[default]
    Ground,
    /// The first `n` bytes of [`MOUSE_PREFIX`] arrived while the VT parser
    /// was in ground state; they are held back because the VT parser would
    /// reject the minus signs of signed SGR pixel coordinates.
    MousePrefix(usize),
    /// Collecting the parameters of an SGR mouse report.
    Mouse(Report),
    /// Collecting a bracketed paste.
    Paste(Paste),
    /// Discarding an oversized control sequence until the VT parser returns
    /// to ground.
    Muted(Recovery),
}

/// Decode signed mouse coordinates before the VT parser rejects their minus
/// signs.
pub(super) struct Parser {
    terminal: VTParser,
    decoder: Decoder,
    events: VecDeque<Event>,
    mode: Mode,
    /// Bytes the VT parser has spent in its current control sequence.
    control_bytes: usize,
    clock: Clock,
}

impl Parser {
    pub(super) fn new() -> Self {
        Self {
            terminal: VTParser::new(),
            decoder: Decoder::default(),
            events: VecDeque::new(),
            mode: Mode::Ground,
            control_bytes: 0,
            clock: Clock::default(),
        }
    }

    /// Whether decoded events are waiting to be popped.
    pub(super) fn pending(&self) -> bool {
        !self.events.is_empty()
    }

    /// Pop the oldest decoded event.
    pub(super) fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Deadline by which an oversized control sequence or bracketed paste
    /// must terminate.
    pub(super) const fn recovery_deadline(&self) -> Option<Instant> {
        match &self.mode {
            Mode::Muted(recovery) | Mode::Paste(Paste::Oversized { recovery, .. }) => {
                Some(recovery.deadline)
            }
            _ => None,
        }
    }

    /// Clock shared with tests that advance time while a read is pending.
    #[cfg(test)]
    pub(super) const fn clock(&self) -> &Clock {
        &self.clock
    }

    fn check_recovery(&self, now: Instant) -> Result<(), InputError> {
        match &self.mode {
            Mode::Muted(recovery) => recovery.check(now, InputError::Control),
            Mode::Paste(Paste::Oversized { recovery, .. }) => {
                recovery.check(now, InputError::Paste)
            }
            _ => Ok(()),
        }
    }

    pub(super) fn check_recovery_now(&self) -> Result<(), InputError> {
        self.check_recovery(self.clock.now())
    }

    fn mute(&mut self) {
        self.mode = Mode::Muted(Recovery::new(self.clock.now()));
        tracing::warn!("Discarding oversized terminal control sequence");
    }

    pub(super) fn push(&mut self, byte: u8) -> Result<(), InputError> {
        match mem::take(&mut self.mode) {
            mode @ Mode::Muted(_) => {
                self.mode = mode;
                self.terminal_byte(byte)
            }
            mode @ Mode::Paste(_) => {
                self.mode = mode;
                self.paste_byte(byte)
            }
            Mode::Mouse(report) => self.mouse_byte(report, byte),
            Mode::MousePrefix(matched) => self.prefix_byte(matched, byte),
            Mode::Ground if self.terminal.is_ground() && byte == ESC => {
                self.mode = Mode::MousePrefix(1);
                Ok(())
            }
            Mode::Ground => self.terminal_byte(byte),
        }
    }

    fn prefix_byte(&mut self, matched: usize, byte: u8) -> Result<(), InputError> {
        if byte == ESC {
            self.mode = Mode::MousePrefix(1);
            return Ok(());
        }
        if MOUSE_PREFIX[matched] == byte {
            self.mode = if matched + 1 == MOUSE_PREFIX.len() {
                Mode::Mouse(Report::new())
            } else {
                Mode::MousePrefix(matched + 1)
            };
            return Ok(());
        }
        for &held in &MOUSE_PREFIX[..matched] {
            self.terminal_byte(held)?;
        }
        self.terminal_byte(byte)
    }

    fn mouse_byte(&mut self, mut report: Report, byte: u8) -> Result<(), InputError> {
        match byte {
            b'M' | b'm' | b'0'..=b'9' | b';' | b'-' if !report.push(byte) => {
                self.discard_mouse(&report, byte)
            }
            b'M' | b'm' => {
                if let Some(event) = mouse(report.as_bytes()) {
                    self.events.push_back(event);
                }
                Ok(())
            }
            b'0'..=b'9' | b';' | b'-' => {
                self.mode = Mode::Mouse(report);
                Ok(())
            }
            _ => self.push(byte),
        }
    }

    /// Hand an oversized mouse report to the VT parser while discarding it,
    /// so recovery ends where the VT parser finds its final byte.
    fn discard_mouse(&mut self, report: &Report, byte: u8) -> Result<(), InputError> {
        self.mute();
        for &byte in report.as_bytes().iter().chain([&byte]) {
            self.terminal_byte(byte)?;
        }
        Ok(())
    }

    /// Collect one bracketed paste byte, discarding a paste that outgrows
    /// [`MAX_TEXT`] while still watching for its terminator.
    fn paste_byte(&mut self, byte: u8) -> Result<(), InputError> {
        let Mode::Paste(paste) = &mut self.mode else {
            return Ok(());
        };
        match paste {
            Paste::Text(bytes) => {
                bytes.push(byte);
                if bytes.ends_with(PASTE_END) {
                    bytes.truncate(bytes.len() - PASTE_END.len());
                    let bytes = mem::take(bytes);
                    self.mode = Mode::Ground;
                    if let Ok(text) = String::from_utf8(bytes) {
                        self.events.push_back(Event::Paste(text));
                    }
                } else if bytes.len() >= MAX_TEXT + PASTE_END.len() {
                    let tail = bytes.split_off(bytes.len() - (PASTE_END.len() - 1));
                    *paste = Paste::Oversized {
                        tail,
                        recovery: Recovery::new(self.clock.now()),
                    };
                    tracing::warn!("Discarding oversized bracketed paste");
                }
            }
            Paste::Oversized { tail, recovery } => {
                tail.push(byte);
                if tail.ends_with(PASTE_END) {
                    recovery.check(self.clock.now(), InputError::Paste)?;
                    self.mode = Mode::Ground;
                } else {
                    recovery.discard(InputError::Paste)?;
                    tail.drain(..tail.len().saturating_sub(PASTE_END.len() - 1));
                }
            }
        }
        Ok(())
    }

    /// Feed one byte to the VT parser while in [`Mode::Ground`] or
    /// [`Mode::Muted`].
    fn terminal_byte(&mut self, byte: u8) -> Result<(), InputError> {
        if !matches!(self.mode, Mode::Muted(_))
            && !self.terminal.is_ground()
            && self.control_bytes >= MAX_CONTROL
        {
            self.mute();
        }
        self.decoder.sink = if matches!(self.mode, Mode::Muted(_)) {
            Sink::Discard
        } else {
            Sink::Collect
        };
        self.terminal.parse_byte(byte, &mut self.decoder);
        for decoded in self.decoder.output.drain(..) {
            match decoded {
                Decoded::Event(event) => self.events.push_back(event),
                Decoded::PasteStart => self.mode = Mode::Paste(Paste::Text(Vec::new())),
            }
        }
        self.control_bytes = if self.terminal.is_ground() {
            0
        } else {
            self.control_bytes.saturating_add(1)
        };
        if let Mode::Muted(recovery) = &mut self.mode {
            if self.terminal.is_ground() {
                recovery.check(self.clock.now(), InputError::Control)?;
                self.mode = Mode::Ground;
            } else {
                recovery.discard(InputError::Control)?;
            }
        }
        Ok(())
    }
}

/// What one VT parser action produced.
enum Decoded {
    Event(Event),
    PasteStart,
}

/// VT parser actions collected for the byte being parsed.
#[derive(Default)]
struct Decoder {
    output: Vec<Decoded>,
    sink: Sink,
}

impl Decoder {
    fn push(&mut self, decoded: Decoded) {
        if self.sink == Sink::Collect {
            self.output.push(decoded);
        }
    }
}

/// Whether the [`Decoder`] keeps what it decodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Sink {
    #[default]
    Collect,
    /// The parser is muted; decoding would only produce discarded output.
    Discard,
}

impl VTActor for Decoder {
    fn csi_dispatch(&mut self, params: &[CsiParam], truncated: bool, byte: u8) {
        if truncated {
            return;
        }
        if params == PASTE_START && byte == b'~' {
            self.push(Decoded::PasteStart);
        } else if let Some(event) = csi(params, byte) {
            self.push(Decoded::Event(event));
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]]) {}

    fn print(&mut self, character: char) {
        if !character.is_control() {
            self.push(Decoded::Event(Event::Text(character.to_string())));
        }
    }

    fn execute_c0_or_c1(&mut self, _control: u8) {}

    fn dcs_hook(&mut self, _mode: u8, _params: &[i64], _intermediates: &[u8], _truncated: bool) {}

    fn dcs_put(&mut self, _byte: u8) {}

    fn dcs_unhook(&mut self) {}

    fn esc_dispatch(
        &mut self,
        _params: &[i64],
        _intermediates: &[u8],
        _truncated: bool,
        _byte: u8,
    ) {
    }

    fn apc_dispatch(&mut self, _data: Vec<u8>) {}
}

#[cfg(test)]
pub(super) mod tests {
    use meowland_protocol::{Button, Modifiers, Transition};

    use super::*;
    use crate::input::{csi::tests::key, event::Focus, mouse::tests::pointer};

    fn parse_result(bytes: &[u8]) -> (Result<(), InputError>, Vec<Event>) {
        let mut parser = Parser::new();
        let mut result = Ok(());
        for &byte in bytes {
            if let Err(error) = parser.push(byte) {
                result = Err(error);
                break;
            }
        }
        (result, parser.events.into_iter().collect())
    }

    fn parse(bytes: &[u8]) -> Vec<Event> {
        let (result, events) = parse_result(bytes);
        result.expect("valid terminal input");
        events
    }

    fn unterminated(prefix: &[u8], filler: u8) -> Vec<u8> {
        let mut bytes = prefix.to_vec();
        bytes.extend(std::iter::repeat_n(filler, MAX_CONTROL + 1));
        bytes
    }

    fn terminated(prefix: &[u8], filler: u8, terminator: &[u8]) -> Vec<u8> {
        let mut bytes = unterminated(prefix, filler);
        bytes.extend_from_slice(terminator);
        bytes.extend_from_slice(b"\x1b[I");
        bytes
    }

    fn oversized_mouse(extra: usize) -> Vec<u8> {
        let mut bytes = MOUSE_PREFIX.to_vec();
        bytes.extend(std::iter::repeat_n(b'1', MAX_MOUSE + 1 + extra));
        bytes
    }

    pub(in crate::input) fn muted_parser() -> Parser {
        let mut parser = Parser::new();
        parser.clock.freeze(Instant::now());
        for &byte in &oversized_mouse(0) {
            parser.push(byte).expect("muted mouse report");
        }
        assert!(
            matches!(parser.mode, Mode::Muted(_)),
            "parser must be muted"
        );
        parser
    }

    fn assert_focus_only(name: &str, bytes: &[u8]) {
        let (result, events) = parse_result(bytes);
        if let Err(error) = result {
            panic!("{name}: {error}");
        }
        assert!(
            matches!(events.as_slice(), [Event::Focus(Focus::Gained)]),
            "{name}: {events:?}"
        );
    }

    fn parse_chunks_after_drain(chunks: &[&[u8]]) -> Vec<Vec<Event>> {
        let mut parser = Parser::new();
        chunks
            .iter()
            .map(|chunk| {
                for &byte in *chunk {
                    parser.push(byte).expect("valid terminal input");
                }
                parser.events.drain(..).collect()
            })
            .collect()
    }

    #[test]
    fn parser_decodes_complete_stream_in_order() {
        let events = parse(b"\x1b[I\x1b[<0;-10;20M\x1b[97;6u\x1b[200~hi\x1b[201~");
        let [focus, pointer_event, key_event, paste] = events.as_slice() else {
            panic!("expected four events, got {events:?}");
        };
        assert!(matches!(focus, Event::Focus(Focus::Gained)));
        let pointer = pointer(pointer_event);
        assert_eq!((pointer.x, pointer.y), (-10.0, 20.0));
        assert_eq!(pointer.button, Some((Button::Left, Transition::Pressed)));
        let key = key(key_event);
        assert_eq!(key.code, u32::from(b'a'));
        assert_eq!(key.modifiers, Modifiers::SHIFT | Modifiers::CONTROL);
        assert!(matches!(paste, Event::Paste(text) if text == "hi"));
    }

    #[test]
    fn parser_preserves_key_state_across_drained_chunks() {
        let events = parse_chunks_after_drain(&[b"\x1b[I\x1b[97;6", b"u"]);
        let [before, after] = events.as_slice() else {
            panic!("expected two chunks, got {events:?}");
        };
        assert!(matches!(before.as_slice(), [Event::Focus(Focus::Gained)]));
        let [event] = after.as_slice() else {
            panic!("expected one key event, got {after:?}");
        };
        let key = key(event);
        assert_eq!(key.code, u32::from(b'a'));
        assert_eq!(key.modifiers, Modifiers::SHIFT | Modifiers::CONTROL);
    }

    #[test]
    fn parser_preserves_mouse_state_across_drained_chunks() {
        let events = parse_chunks_after_drain(&[b"\x1b[I\x1b[<0;-10;", b"20M"]);
        let [before, after] = events.as_slice() else {
            panic!("expected two chunks, got {events:?}");
        };
        assert!(matches!(before.as_slice(), [Event::Focus(Focus::Gained)]));
        let [event] = after.as_slice() else {
            panic!("expected one pointer event, got {after:?}");
        };
        let pointer = pointer(event);
        assert_eq!((pointer.x, pointer.y), (-10.0, 20.0));
        assert_eq!(pointer.button, Some((Button::Left, Transition::Pressed)));
    }

    #[test]
    fn parser_keeps_escape_prefix_ambiguity() {
        assert!(matches!(
            parse(b"\x1b[I").as_slice(),
            [Event::Focus(Focus::Gained)]
        ));
        assert!(matches!(
            parse(b"\x1b\x1b[I").as_slice(),
            [Event::Focus(Focus::Gained)]
        ));

        let events = parse(b"\x1b[<0;1;2M\x1b[I");
        assert_eq!(events.len(), 2);
        assert_eq!(
            pointer(&events[0]).button,
            Some((Button::Left, Transition::Pressed))
        );
        assert!(matches!(events[1], Event::Focus(Focus::Gained)));

        let events = parse(b"\x1b[<0;1;2X\x1b[I");
        assert!(
            matches!(&events[..], [Event::Text(text), Event::Focus(Focus::Gained)] if text == "X")
        );
    }

    #[test]
    fn parser_preserves_utf8_state_across_drained_chunks() {
        let events = parse_chunks_after_drain(&[b"a\xc3", b"\xa9"]);
        let [before, after] = events.as_slice() else {
            panic!("expected two chunks, got {events:?}");
        };
        assert!(matches!(before.as_slice(), [Event::Text(text)] if text == "a"));
        assert!(matches!(after.as_slice(), [Event::Text(text)] if text == "é"));
    }

    #[test]
    fn parser_preserves_paste_state_across_drained_chunks() {
        let events = parse_chunks_after_drain(&[b"x\x1b[200~hel", b"lo\x1b[201~"]);
        let [before, after] = events.as_slice() else {
            panic!("expected two chunks, got {events:?}");
        };
        assert!(matches!(before.as_slice(), [Event::Text(text)] if text == "x"));
        assert!(matches!(after.as_slice(), [Event::Paste(text)] if text == "hello"));
    }

    #[test]
    fn parser_rejects_invalid_utf8_paste() {
        let events = parse(b"\x1b[200~\xff\x1b[201~x");
        assert!(matches!(&events[..], [Event::Text(text)] if text == "x"));
    }

    #[test]
    fn parser_bounds_bracketed_paste() {
        let payload = vec![b'a'; meowland_protocol::clipboard::MAX_TEXT];
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(b"\x1b[201~");
        let events = parse(&bytes);
        assert!(matches!(&events[..], [Event::Paste(text)] if text.len() == payload.len()));

        let payload = vec![b'a'; meowland_protocol::clipboard::MAX_TEXT + 1];
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(b"\x1b[201~\x1b[I");
        let events = parse(&bytes);
        assert!(matches!(&events[..], [Event::Focus(Focus::Gained)]));
    }

    #[test]
    fn parser_discards_oversized_control_sequences_until_their_terminator() {
        let cases = [
            ("CSI final", terminated(b"\x1b[", b'1', b"m")),
            ("OSC ST", terminated(b"\x1b]", b'x', b"\x1b\\")),
            ("OSC BEL", terminated(b"\x1b]", b'x', b"\x07")),
            ("OSC C1 ST", terminated(b"\x1b]", b'x', b"\x9c")),
            ("DCS ST", terminated(b"\x1bP", b'x', b"\x1b\\")),
            ("APC ST", terminated(b"\x1b_", b'x', b"\x1b\\")),
            ("UTF-8 C1 OSC ST", terminated(b"\xc2\x9d", b'x', b"\x1b\\")),
        ];
        for (name, bytes) in cases {
            assert_focus_only(name, &bytes);
        }

        let mut interrupted = unterminated(b"\x1b[", b'1');
        interrupted.extend_from_slice(b"\x1b]interrupted\x07\x1b[I");
        assert_focus_only("CSI interrupted by OSC", &interrupted);
    }

    #[test]
    fn parser_recovers_when_oversized_control_sequences_are_cancelled() {
        let cases: [(&str, &[u8], u8, u8); 4] = [
            ("CSI cancelled by SUB", b"\x1b[", b'1', 0x1a),
            ("OSC cancelled by CAN", b"\x1b]", b'x', 0x18),
            ("DCS cancelled by SUB", b"\x1bP", b'x', 0x1a),
            ("APC cancelled by CAN", b"\x1b_", b'x', 0x18),
        ];
        for (name, prefix, filler, cancel) in cases {
            let mut bytes = unterminated(prefix, filler);
            bytes.push(cancel);
            bytes.extend_from_slice(b"\x1b[I");
            assert_focus_only(name, &bytes);
        }
    }

    #[test]
    fn parser_recovers_when_st_straddles_the_control_limit() {
        for payload in [
            MAX_CONTROL - 3,
            MAX_CONTROL - 2,
            MAX_CONTROL - 1,
            MAX_CONTROL,
            MAX_CONTROL + 1,
        ] {
            let mut bytes = b"\x1b]".to_vec();
            bytes.extend(std::iter::repeat_n(b'x', payload));
            bytes.extend_from_slice(b"\x1b\\\x1b[I");
            assert_focus_only(&format!("OSC payload {payload}"), &bytes);
        }
    }

    #[test]
    fn parser_recovers_when_mouse_reports_exceed_the_fast_path() {
        let mut terminated = oversized_mouse(0);
        terminated.extend_from_slice(b"M\x1b[I");
        assert_focus_only("mouse M", &terminated);

        for (name, cancel) in [("mouse CAN", 0x18), ("mouse SUB", 0x1a)] {
            let mut bytes = oversized_mouse(0);
            bytes.push(cancel);
            bytes.extend_from_slice(b"\x1b[I");
            assert_focus_only(name, &bytes);
        }
    }

    #[test]
    fn parser_fails_closed_when_oversized_mouse_never_terminates() {
        let mut bytes = oversized_mouse(MAX_DISCARDED);
        bytes.extend_from_slice(b"\x1b[I");
        let (result, events) = parse_result(&bytes);
        let error = result.expect_err("unterminated mouse report must fail");
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(io::Error::from(error).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parser_enforces_the_muted_recovery_deadline() {
        let parser = muted_parser();
        let deadline = parser
            .recovery_deadline()
            .expect("muted after mouse overflow");
        let before = deadline
            .checked_sub(Duration::from_millis(1))
            .expect("deadline has margin");
        assert!(parser.check_recovery(before).is_ok());
        assert_eq!(parser.check_recovery(deadline), Err(InputError::Control));

        let mut recovered = muted_parser();
        recovered.push(b'M').expect("terminated mouse report");
        assert!(!matches!(recovered.mode, Mode::Muted(_)));
        for &byte in b"\x1b[I" {
            recovered.push(byte).expect("input after recovery");
        }
        assert!(matches!(
            recovered.events.front(),
            Some(Event::Focus(Focus::Gained))
        ));
        assert_eq!(recovered.events.len(), 1);
    }

    #[test]
    fn parser_fails_closed_when_a_terminator_arrives_after_the_recovery_deadline() {
        let mut parser = muted_parser();
        let deadline = parser
            .recovery_deadline()
            .expect("muted after mouse overflow");
        parser.clock.freeze(deadline);
        assert_eq!(parser.push(b'M'), Err(InputError::Control));
        assert!(
            matches!(parser.mode, Mode::Muted(_)),
            "an expired deadline must not be cleared"
        );
        assert!(parser.events.is_empty());
    }

    #[test]
    fn parser_fails_closed_when_oversized_control_never_terminates() {
        let mut bytes = unterminated(b"\x1b]", b'x');
        bytes.extend(std::iter::repeat_n(b'x', MAX_DISCARDED + 1));
        bytes.extend_from_slice(b"\x1b[I");
        let (result, events) = parse_result(&bytes);
        let error = result.expect_err("unterminated control must fail");
        assert!(events.is_empty(), "{events:?}");
        let error = io::Error::from(error);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("unterminated control sequence"),
            "{error}"
        );
    }

    fn push_all(parser: &mut Parser, bytes: &[u8]) -> Result<(), InputError> {
        bytes.iter().try_for_each(|&byte| parser.push(byte))
    }

    fn oversized_paste_parser() -> Parser {
        let mut parser = Parser::new();
        parser.clock.freeze(Instant::now());
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_TEXT + PASTE_END.len()));
        push_all(&mut parser, &bytes).expect("oversized paste");
        assert!(
            parser.recovery_deadline().is_some(),
            "oversized paste must be discarded under a deadline"
        );
        parser
    }

    #[test]
    fn parser_recovers_when_an_oversized_paste_terminator_is_split() {
        for split in 1..PASTE_END.len() {
            let mut parser = oversized_paste_parser();
            push_all(&mut parser, &PASTE_END[..split]).expect("partial terminator");
            push_all(&mut parser, &PASTE_END[split..]).expect("rest of terminator");
            assert_eq!(parser.recovery_deadline(), None, "split {split}");
            push_all(&mut parser, b"x\x1b[I").expect("input after paste");
            let events: Vec<Event> = std::iter::from_fn(|| parser.next_event()).collect();
            assert_eq!(
                events,
                [Event::Text("x".to_owned()), Event::Focus(Focus::Gained)],
                "split {split}"
            );
        }
    }

    #[test]
    fn parser_fails_closed_when_an_oversized_paste_outlives_its_deadline() {
        let mut parser = oversized_paste_parser();
        let deadline = parser
            .recovery_deadline()
            .expect("discarding oversized paste");
        parser.clock.freeze(deadline);
        assert_eq!(parser.check_recovery_now(), Err(InputError::Paste));
        assert_eq!(push_all(&mut parser, PASTE_END), Err(InputError::Paste));
        assert_eq!(
            parser.recovery_deadline(),
            Some(deadline),
            "an expired paste must not be cleared"
        );
        assert!(!parser.pending());
    }

    #[test]
    fn parser_fails_closed_when_an_oversized_paste_never_terminates() {
        let mut parser = oversized_paste_parser();
        let mut bytes = vec![b'a'; MAX_DISCARDED + 1];
        bytes.extend_from_slice(PASTE_END);
        bytes.extend_from_slice(b"\x1b[I");
        assert_eq!(push_all(&mut parser, &bytes), Err(InputError::Paste));
        assert!(!parser.pending());
    }

    #[test]
    fn parser_ignores_osc_strings_however_they_end() {
        let terminators: [&[u8]; 7] = [
            b"\x07",
            b"\x9c",
            b"\xc2\x9c",
            b"\x1b\\",
            b"\x18",
            b"\x1a",
            b"\x1b",
        ];
        for terminator in terminators {
            let mut bytes = b"\x1b]52;c;aGk=".to_vec();
            bytes.extend_from_slice(terminator);
            bytes.extend_from_slice(b"\x1b[I");
            for cut in 0..=bytes.len() {
                let chunks: [&[u8]; 2] = bytes.split_at(cut).into();
                let events: Vec<Event> = parse_chunks_after_drain(&chunks)
                    .into_iter()
                    .flatten()
                    .collect();
                assert_eq!(
                    events,
                    [Event::Focus(Focus::Gained)],
                    "{terminator:?} cut at {cut}"
                );
            }
        }
    }
}
