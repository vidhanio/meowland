//! Nonblocking terminal input with signed SGR pixel mouse decoding and
//! `vtparse`.

#[cfg(test)]
use std::{cell::Cell, rc::Rc};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use calloop::LoopHandle;
use futures::io::AsyncReadExt;
use meowland_async::asynchronous::Descriptor;
use meowland_protocol::{Button, Input, Modifiers, Scroll, Transition, clipboard::MAX_TEXT};
use rustix::fs::{Mode, OFlags, open};
use vtparse::{CsiParam, VTActor, VTParser};

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

const MAX_CONTROL: usize = MAX_TEXT.div_ceil(3) * 4 + 64;
const MAX_MUTED: usize = MAX_CONTROL * 4;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);
const MOUSE_PREFIX: &[u8] = b"\x1b[<";
const MAX_MOUSE: usize = 64;
const PASTE_END: &[u8] = b"\x1b[201~";
/// SGR bit marking a wheel report rather than a button or motion report.
const WHEEL: u32 = 64;
/// SGR bit marking pointer motion.
const MOTION: u32 = 32;
/// SGR bit marking buttons above the first three, reported as numbers from 8.
const EXTRA: u32 = 128;
/// First X11 button number carried through the [`EXTRA`] bit.
const EXTRA_BUTTON: u8 = 8;
/// Wheel distance the terminal reports for one detent.
const SCROLL_DETENT: i16 = 120;

/// Key transition reported by the Kitty keyboard protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Initial press, before any autorepeat events.
    Press,
    /// Autorepeat while the key remains held.
    Repeat,
    /// Release of a previously held key.
    Release,
}

/// Logical key identity, modifier state, and associated text reported by the
/// Kitty protocol.
#[derive(Clone, Debug)]
pub struct Key {
    /// Unicode code point or Kitty functional-key number.
    pub code: u32,
    /// Modifier bits using [`Modifiers`].
    pub modifiers: Modifiers,
    /// Press, repeat, or release transition.
    pub kind: Kind,
    /// Text the terminal associated with the transition, if reported.
    pub text: Option<String>,
}

/// Decoded terminal event, with bounded text and control-sequence payloads.
#[derive(Debug)]
pub enum Event {
    /// Kitty keyboard transition for the compositor to resolve.
    Key(Key),
    /// Committed Unicode text that arrived outside a key event.
    Text(String),
    /// Pixel-accurate pointer input, including signed drag coordinates.
    Pointer(Input),
    /// Whether the terminal pane gained focus.
    Focus(bool),
    /// Kernel-reported terminal size changed.
    Resize,
    /// Completed bracketed paste containing valid UTF-8.
    Paste(String),
    /// Kitty OSC 5522 metadata and optional payload.
    Clipboard(Vec<u8>),
    /// Base64 payload from an OSC 52 text reply.
    ClipboardText(Vec<u8>),
}

/// Nonblocking terminal input descriptor with incremental, bounded VT decoding.
pub struct Reader {
    fd: Descriptor,
    parser: Parser,
    bytes: Box<[u8]>,
}

impl Reader {
    /// Open terminal input without changing stdin's blocking flags.
    ///
    /// # Errors
    /// Returns an error if opening, registering, or watching input fails.
    pub fn new<D>(handle: &LoopHandle<'static, D>) -> io::Result<Self> {
        let fd = open(
            "/proc/self/fd/0",
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        let fd = Descriptor::new(fd, handle)?;
        Ok(Self {
            fd,
            parser: Parser::new(),
            bytes: vec![0; 16 * 1024].into_boxed_slice(),
        })
    }

    /// Whether decoded events are ready without another descriptor read.
    #[must_use]
    pub fn pending(&self) -> bool {
        !self.parser.decoder.events.is_empty()
    }

    /// Deadline by which a muted control sequence must return to ground.
    #[must_use]
    pub const fn recovery_deadline(&self) -> Option<Instant> {
        self.parser.muted_deadline
    }

    /// Fail if a muted control sequence has exceeded its recovery deadline.
    ///
    /// # Errors
    /// Returns an input error when the recovery deadline has elapsed.
    pub fn check_recovery(&self) -> io::Result<()> {
        self.parser.check_recovery_now()?;
        Ok(())
    }

    /// Pop a decoded terminal event.
    pub fn next_event(&mut self) -> Option<Event> {
        self.parser.decoder.events.pop_front()
    }

    /// Read and decode the next bounded input chunk.
    ///
    /// # Errors
    /// Returns an error on EOF, input failure, or an unterminated control
    /// sequence that exceeds the recovery limit, including one whose deadline
    /// elapsed while this read was waiting.
    #[expect(
        clippy::future_not_send,
        reason = "terminal input belongs to the calloop owner thread"
    )]
    pub async fn read_ready(&mut self) -> io::Result<()> {
        self.check_recovery()?;
        if !self.parser.decoder.events.is_empty() {
            return Ok(());
        }
        let read = self.fd.read(&mut self.bytes).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.check_recovery()?;
        for &byte in &self.bytes[..read] {
            self.parser.push(byte)?;
        }
        Ok(())
    }

    /// Read raw capability replies before starting event decoding.
    ///
    /// # Errors
    /// Returns an error if terminal input fails.
    #[expect(
        clippy::future_not_send,
        reason = "terminal input belongs to the calloop owner thread"
    )]
    pub async fn read_raw(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.fd.read(bytes).await
    }
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

/// Time source for recovery deadlines that tests can freeze.
#[derive(Clone, Debug)]
struct Clock {
    source: fn() -> Instant,
    #[cfg(test)]
    frozen: Rc<Cell<Option<Instant>>>,
}

impl Clock {
    fn new() -> Self {
        Self {
            source: Instant::now,
            #[cfg(test)]
            frozen: Rc::new(Cell::new(None)),
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.frozen.get() {
            return now;
        }
        (self.source)()
    }

    #[cfg(test)]
    fn freeze(&self, now: Instant) {
        self.frozen.set(Some(now));
    }
}

/// Decode signed mouse coordinates before the VT parser rejects their minus
/// signs.
struct Parser {
    terminal: VTParser,
    decoder: Decoder,
    mouse: Vec<u8>,
    control_bytes: usize,
    muted_bytes: usize,
    muted_deadline: Option<Instant>,
    clock: Clock,
}

/// An oversized control sequence that did not return to ground within the
/// recovery limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InputError;

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "terminal input contained an unterminated control sequence that exceeded the recovery limit"
        )
    }
}

impl std::error::Error for InputError {}

impl From<InputError> for io::Error {
    fn from(error: InputError) -> Self {
        Self::new(io::ErrorKind::InvalidData, error)
    }
}

impl Parser {
    fn new() -> Self {
        Self {
            terminal: VTParser::new(),
            decoder: Decoder::default(),
            mouse: Vec::new(),
            control_bytes: 0,
            muted_bytes: 0,
            muted_deadline: None,
            clock: Clock::new(),
        }
    }

    fn begin_muted(&mut self) {
        if self.decoder.muted {
            return;
        }
        self.decoder.muted = true;
        self.muted_bytes = 0;
        self.muted_deadline = Some(self.clock.now() + RECOVERY_TIMEOUT);
        tracing::warn!("Discarding oversized terminal control sequence");
    }

    fn end_muted(&mut self) -> Result<(), InputError> {
        self.check_recovery_now()?;
        self.decoder.muted = false;
        self.muted_bytes = 0;
        self.muted_deadline = None;
        Ok(())
    }

    fn check_recovery(&self, now: Instant) -> Result<(), InputError> {
        if self.muted_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(InputError);
        }
        Ok(())
    }

    fn check_recovery_now(&self) -> Result<(), InputError> {
        self.check_recovery(self.clock.now())
    }

    fn discard_mouse(&mut self, byte: u8) -> Result<(), InputError> {
        let mut pending = std::mem::take(&mut self.mouse);
        self.begin_muted();
        pending.push(byte);
        for byte in pending {
            self.terminal_byte(byte)?;
        }
        Ok(())
    }

    fn push(&mut self, byte: u8) -> Result<(), InputError> {
        if self.decoder.muted {
            return self.terminal_byte(byte);
        }
        if self.decoder.paste.is_some() {
            self.decoder.paste_byte(byte);
            return Ok(());
        }
        if self.mouse.starts_with(MOUSE_PREFIX) {
            match byte {
                b'M' | b'm' => {
                    if self.mouse.len() < MAX_MOUSE {
                        self.mouse.push(byte);
                        if let Some(event) = mouse(&self.mouse) {
                            self.decoder.events.push_back(event);
                        }
                        self.mouse.clear();
                    } else {
                        return self.discard_mouse(byte);
                    }
                }
                b'0'..=b'9' | b';' | b'-' => {
                    if self.mouse.len() < MAX_MOUSE {
                        self.mouse.push(byte);
                    } else {
                        return self.discard_mouse(byte);
                    }
                }
                _ => {
                    self.mouse.clear();
                    return self.push(byte);
                }
            }
            return Ok(());
        }
        if !self.mouse.is_empty() {
            if byte == b'\x1b' {
                self.mouse.truncate(1);
                return Ok(());
            }
            self.mouse.push(byte);
            if MOUSE_PREFIX.starts_with(&self.mouse) {
                return Ok(());
            }
            let mut pending = std::mem::take(&mut self.mouse);
            for &byte in &pending {
                self.terminal_byte(byte)?;
            }
            pending.clear();
            self.mouse = pending;
            return Ok(());
        }
        if self.terminal.is_ground() && byte == b'\x1b' {
            self.mouse.push(byte);
            Ok(())
        } else {
            self.terminal_byte(byte)
        }
    }

    fn terminal_byte(&mut self, byte: u8) -> Result<(), InputError> {
        if !self.decoder.muted && !self.terminal.is_ground() && self.control_bytes >= MAX_CONTROL {
            self.begin_muted();
        }
        self.terminal.parse_byte(byte, &mut self.decoder);
        self.control_bytes = if self.terminal.is_ground() {
            0
        } else {
            self.control_bytes.saturating_add(1)
        };
        if self.decoder.muted {
            self.muted_bytes += 1;
            if self.terminal.is_ground() {
                self.end_muted()?;
            } else if self.muted_bytes > MAX_MUTED {
                return Err(InputError);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Decoder {
    events: VecDeque<Event>,
    paste: Option<Paste>,
    muted: bool,
}

#[derive(Default)]
struct Paste {
    bytes: Vec<u8>,
    oversized: bool,
}

impl Paste {
    fn push(&mut self, byte: u8) -> bool {
        self.bytes.push(byte);
        if self.bytes.ends_with(PASTE_END) {
            self.bytes.truncate(self.bytes.len() - PASTE_END.len());
            return true;
        }
        if self.oversized || self.bytes.len() > MAX_TEXT + PASTE_END.len() {
            self.oversized = true;
            self.bytes
                .drain(..self.bytes.len().saturating_sub(PASTE_END.len() - 1));
        }
        false
    }

    fn finish(self) -> Option<String> {
        if self.oversized || self.bytes.len() > MAX_TEXT {
            return None;
        }
        String::from_utf8(self.bytes).ok()
    }
}

impl Decoder {
    fn paste_byte(&mut self, byte: u8) {
        if self.muted {
            return;
        }
        if let Some(paste) = &mut self.paste
            && paste.push(byte)
            && let Some(text) = self.paste.take().and_then(Paste::finish)
        {
            self.events.push_back(Event::Paste(text));
        }
    }
}

impl VTActor for Decoder {
    fn csi_dispatch(&mut self, params: &[CsiParam], truncated: bool, byte: u8) {
        if self.muted || truncated {
            return;
        }
        if params == [CsiParam::Integer(200)] && byte == b'~' {
            self.paste = Some(Paste::default());
        } else if let Some(event) = csi(params, byte) {
            self.events.push_back(event);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
        if self.muted {
            return;
        }
        if let [b"5522", metadata, rest @ ..] = params
            && rest.len() <= 1
        {
            let mut packet = metadata.to_vec();
            if let Some(payload) = rest.first() {
                packet.push(b';');
                packet.extend_from_slice(payload);
            }
            self.events.push_back(Event::Clipboard(packet));
        } else if let [b"52", b"c" | b"", payload] = params {
            self.events
                .push_back(Event::ClipboardText(payload.to_vec()));
        }
    }

    fn print(&mut self, character: char) {
        if !self.muted && !character.is_control() {
            self.events.push_back(Event::Text(character.to_string()));
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

/// Decode exactly one nonnegative CSI integer fitting in `u32`.
#[must_use]
pub fn number(params: &[CsiParam]) -> Option<u32> {
    match params {
        [CsiParam::Integer(value)] => u32::try_from(*value).ok(),
        _ => None,
    }
}

fn csi(params: &[CsiParam], final_byte: u8) -> Option<Event> {
    if params.is_empty() {
        match final_byte {
            b'I' => return Some(Event::Focus(true)),
            b'O' => return Some(Event::Focus(false)),
            _ => {}
        }
    }
    let mut fields = params.split(|param| *param == CsiParam::P(b';'));
    let key = fields
        .next()?
        .split(|param| *param == CsiParam::P(b':'))
        .next()?;
    let mut modifier = fields
        .next()
        .unwrap_or(&[])
        .split(|param| *param == CsiParam::P(b':'));
    let bits = modifier
        .next()
        .filter(|params| !params.is_empty())
        .map_or(Some(1), number)?
        .checked_sub(1)?;
    let bits = u8::try_from(bits).ok()?;
    let modifiers = Modifiers::from_kitty(bits);
    let kind = match modifier
        .next()
        .filter(|params| !params.is_empty())
        .map_or(Some(1), number)?
    {
        1 => Kind::Press,
        2 => Kind::Repeat,
        3 => Kind::Release,
        _ => return None,
    };
    let text = if final_byte == b'u' {
        fields.next().and_then(committed_text)
    } else {
        None
    };
    let code = match final_byte {
        b'u' => number(key)?,
        b'A' => 57352,
        b'B' => 57353,
        b'C' => 57351,
        b'D' => 57350,
        b'H' => 57356,
        b'F' => 57357,
        b'E' => 57427,
        b'P' => 57364,
        b'Q' => 57365,
        b'R' => 57366,
        b'S' => 57367,
        b'~' => match number(key)? {
            2 => 57348,
            3 => 57349,
            5 => 57354,
            6 => 57355,
            1 | 7 => 57356,
            4 | 8 => 57357,
            57427 => 57427,
            11..=15 => 57364 + number(key)? - 11,
            17..=21 => 57369 + number(key)? - 17,
            23..=24 => 57374 + number(key)? - 23,
            _ => return None,
        },
        _ => return None,
    };
    if code == 0 {
        return match kind {
            Kind::Press | Kind::Repeat => text.map(Event::Text),
            Kind::Release => None,
        };
    }
    Some(Event::Key(Key {
        code,
        modifiers,
        kind,
        text,
    }))
}

fn committed_text(params: &[CsiParam]) -> Option<String> {
    let mut text = String::new();
    for param in params.split(|param| *param == CsiParam::P(b':')) {
        let character = char::from_u32(number(param)?)?;
        if character.is_control() {
            return None;
        }
        text.push(character);
    }
    (!text.is_empty() && text.len() <= meowland_protocol::MAX_INPUT_TEXT).then_some(text)
}

fn mouse(report: &[u8]) -> Option<Event> {
    let (&final_byte, fields) = report.strip_prefix(MOUSE_PREFIX)?.split_last()?;
    let released = match final_byte {
        b'M' => false,
        b'm' => true,
        _ => return None,
    };
    let mut fields = std::str::from_utf8(fields).ok()?.split(';');
    let code: u32 = fields.next()?.parse().ok()?;
    let x: i32 = fields.next()?.parse().ok()?;
    let y: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let (button, scroll) = if code & WHEEL != 0 {
        (None, wheel(code))
    } else if code & MOTION != 0 {
        (None, Scroll::default())
    } else {
        let state = if released {
            Transition::Released
        } else {
            Transition::Pressed
        };
        (Some((button(code)?, state)), Scroll::default())
    };
    Some(Event::Pointer(Input::Pointer {
        x: f64::from(x),
        y: f64::from(y),
        button,
        scroll,
    }))
}

/// Decode the wheel direction of a report with the [`WHEEL`] bit set.
fn wheel(code: u32) -> Scroll {
    let mut scroll = Scroll::default();
    match code & 3 {
        0 => scroll.vertical = SCROLL_DETENT,
        1 => scroll.vertical = -SCROLL_DETENT,
        2 => scroll.horizontal = -SCROLL_DETENT,
        _ => scroll.horizontal = SCROLL_DETENT,
    }
    scroll
}

/// Decode a button report into its X11 identity, rejecting reserved numbers.
fn button(code: u32) -> Option<Button> {
    let number = u8::try_from(code & 3).ok()?;
    if code & EXTRA != 0 {
        return Some(match number {
            0 => Button::Back,
            1 => Button::Forward,
            other => Button::Other(EXTRA_BUTTON + other),
        });
    }
    match number {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::Write,
        os::unix::net::UnixStream,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    use super::*;

    fn parse_result(bytes: &[u8]) -> (Result<(), InputError>, Vec<Event>) {
        let mut parser = Parser::new();
        let mut result = Ok(());
        for &byte in bytes {
            if let Err(error) = parser.push(byte) {
                result = Err(error);
                break;
            }
        }
        (result, parser.decoder.events.into_iter().collect())
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

    fn muted_parser() -> Parser {
        let mut parser = Parser::new();
        parser.clock.freeze(Instant::now());
        for &byte in &oversized_mouse(0) {
            parser.push(byte).expect("muted mouse report");
        }
        assert!(parser.muted_deadline.is_some(), "parser must be muted");
        parser
    }

    fn assert_focus_only(name: &str, bytes: &[u8]) {
        let (result, events) = parse_result(bytes);
        if let Err(error) = result {
            panic!("{name}: {error}");
        }
        assert!(
            matches!(events.as_slice(), [Event::Focus(true)]),
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
                parser.decoder.events.drain(..).collect()
            })
            .collect()
    }

    fn key(event: &Event) -> &Key {
        match event {
            Event::Key(key) => key,
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[derive(Debug, PartialEq)]
    struct Pointer {
        x: f64,
        y: f64,
        button: Option<(Button, Transition)>,
        scroll: Scroll,
    }

    fn pointer(event: &Event) -> Pointer {
        match event {
            Event::Pointer(Input::Pointer {
                x,
                y,
                button,
                scroll,
            }) => Pointer {
                x: *x,
                y: *y,
                button: *button,
                scroll: *scroll,
            },
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }

    fn text_params(length: usize) -> Vec<CsiParam> {
        let mut params = vec![
            CsiParam::Integer(97),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b';'),
        ];
        for index in 0..length {
            if index > 0 {
                params.push(CsiParam::P(b':'));
            }
            params.push(CsiParam::Integer(97));
        }
        params
    }

    #[test]
    fn csi_decodes_focus_and_rejects_parameterized_focus() {
        assert!(matches!(csi(&[], b'I'), Some(Event::Focus(true))));
        assert!(matches!(csi(&[], b'O'), Some(Event::Focus(false))));
        assert!(csi(&[CsiParam::Integer(1)], b'I').is_none());
        assert!(csi(&[], b'Z').is_none());
    }

    #[test]
    fn csi_maps_every_kitty_modifier_bit() {
        let cases = [
            (0, Modifiers::empty()),
            (1, Modifiers::SHIFT),
            (2, Modifiers::ALT),
            (4, Modifiers::CONTROL),
            (8, Modifiers::SUPER),
            (16, Modifiers::HYPER),
            (32, Modifiers::META),
            (64, Modifiers::CAPS_LOCK),
            (128, Modifiers::NUM_LOCK),
            (3, Modifiers::SHIFT | Modifiers::ALT),
            (5, Modifiers::SHIFT | Modifiers::CONTROL),
        ];
        for (bits, expected) in cases {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(i64::from(bits) + 1),
            ];
            let event = csi(&params, b'u').expect("valid Kitty key event");
            let key = key(&event);
            assert_eq!(key.code, u32::from(b'a'), "Kitty bits {bits}");
            assert_eq!(key.modifiers, expected, "Kitty bits {bits}");
            assert_eq!(key.kind, Kind::Press, "Kitty bits {bits}");
            assert_eq!(key.text, None, "Kitty bits {bits}");
        }
    }

    #[test]
    fn csi_decodes_key_transitions() {
        let cases = [(1, Kind::Press), (2, Kind::Repeat), (3, Kind::Release)];
        for (encoded, expected) in cases {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b':'),
                CsiParam::Integer(encoded),
            ];
            let event = csi(&params, b'u').expect("valid Kitty transition");
            assert_eq!(key(&event).kind, expected, "encoded kind {encoded}");
        }
        for encoded in [0, 4, 99] {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b':'),
                CsiParam::Integer(encoded),
            ];
            assert!(
                csi(&params, b'u').is_none(),
                "encoded kind {encoded} must be rejected"
            );
        }
    }

    #[test]
    fn csi_decodes_associated_text() {
        let cases: [(&[CsiParam], &str); 3] = [
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::Integer(1),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                ],
                "a",
            ),
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::Integer(1),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                    CsiParam::P(b':'),
                    CsiParam::Integer(98),
                ],
                "ab",
            ),
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                ],
                "a",
            ),
        ];
        for (params, expected) in cases {
            let event = csi(params, b'u').expect("valid Kitty key event");
            assert_eq!(key(&event).text.as_deref(), Some(expected));
        }
    }

    #[test]
    fn csi_bounds_associated_text() {
        let exact = text_params(meowland_protocol::MAX_INPUT_TEXT);
        let event = csi(&exact, b'u').expect("valid Kitty key event");
        assert_eq!(
            key(&event).text.as_deref().map(str::len),
            Some(meowland_protocol::MAX_INPUT_TEXT)
        );

        let oversized = text_params(meowland_protocol::MAX_INPUT_TEXT + 1);
        let event = csi(&oversized, b'u').expect("valid Kitty key event");
        assert_eq!(key(&event).text, None);
    }

    #[test]
    fn csi_rejects_invalid_associated_text() {
        let cases: [&[CsiParam]; 4] = [
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0x7f),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0x11_0000),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
            ],
        ];
        for params in cases {
            let event = csi(params, b'u').expect("text is optional for key events");
            assert_eq!(key(&event).text, None, "params {params:?}");
        }
    }

    #[test]
    fn csi_turns_text_only_events_into_committed_text() {
        let params = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(matches!(csi(&params, b'u'), Some(Event::Text(text)) if text == "a"));

        let repeat = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b':'),
            CsiParam::Integer(2),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(matches!(csi(&repeat, b'u'), Some(Event::Text(text)) if text == "a"));

        let release = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b':'),
            CsiParam::Integer(3),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(csi(&release, b'u').is_none());
        assert!(csi(&[CsiParam::Integer(0)], b'u').is_none());
    }

    #[test]
    fn csi_decodes_functional_keys() {
        let csi_cases = [
            (b'A', 57352),
            (b'B', 57353),
            (b'C', 57351),
            (b'D', 57350),
            (b'H', 57356),
            (b'F', 57357),
            (b'E', 57427),
            (b'P', 57364),
            (b'Q', 57365),
            (b'R', 57366),
            (b'S', 57367),
        ];
        for (byte, expected) in csi_cases {
            let event = csi(&[], byte).expect("valid functional key");
            assert_eq!(key(&event).code, expected, "final byte {byte:?}");
        }

        let tilde_cases = [
            (1, 57356),
            (2, 57348),
            (3, 57349),
            (4, 57357),
            (5, 57354),
            (6, 57355),
            (7, 57356),
            (8, 57357),
            (11, 57364),
            (15, 57368),
            (17, 57369),
            (21, 57373),
            (23, 57374),
            (24, 57375),
            (57427, 57427),
        ];
        for (number, expected) in tilde_cases {
            let event = csi(&[CsiParam::Integer(number)], b'~').expect("valid tilde key");
            assert_eq!(key(&event).code, expected, "tilde {number}");
        }
        for number in [0, 9, 10, 16, 22, 25, 57426] {
            assert!(
                csi(&[CsiParam::Integer(number)], b'~').is_none(),
                "tilde {number} must be rejected"
            );
        }
    }

    #[test]
    fn csi_rejects_malformed_parameters() {
        let cases: [&[CsiParam]; 5] = [
            &[CsiParam::Integer(-1)],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(0),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(300),
            ],
            &[CsiParam::P(b';')],
            &[],
        ];
        for params in cases {
            assert!(csi(params, b'u').is_none(), "params {params:?}");
        }
        assert!(csi(&[], b'Z').is_none());
    }

    #[test]
    fn mouse_decodes_buttons_and_signed_coordinates() {
        struct Case {
            report: &'static [u8],
            button: Option<(Button, Transition)>,
            x: f64,
            y: f64,
        }

        let cases = [
            Case {
                report: b"\x1b[<0;10;20M",
                button: Some((Button::Left, Transition::Pressed)),
                x: 10.0,
                y: 20.0,
            },
            Case {
                report: b"\x1b[<0;10;20m",
                button: Some((Button::Left, Transition::Released)),
                x: 10.0,
                y: 20.0,
            },
            Case {
                report: b"\x1b[<1;-5;-20M",
                button: Some((Button::Middle, Transition::Pressed)),
                x: -5.0,
                y: -20.0,
            },
            Case {
                report: b"\x1b[<2;0;0m",
                button: Some((Button::Right, Transition::Released)),
                x: 0.0,
                y: 0.0,
            },
            Case {
                report: b"\x1b[<128;1;2M",
                button: Some((Button::Back, Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<129;1;2M",
                button: Some((Button::Forward, Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<130;1;2M",
                button: Some((Button::Other(10), Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<131;1;2m",
                button: Some((Button::Other(11), Transition::Released)),
                x: 1.0,
                y: 2.0,
            },
        ];
        for case in cases {
            let event = mouse(case.report).expect("valid mouse report");
            let pointer = pointer(&event);
            assert_eq!(pointer.x, case.x, "report {:?}", case.report);
            assert_eq!(pointer.y, case.y, "report {:?}", case.report);
            assert_eq!(pointer.button, case.button, "report {:?}", case.report);
            assert_eq!(
                pointer.scroll,
                Scroll::default(),
                "report {:?}",
                case.report
            );
        }
    }

    #[test]
    fn mouse_decodes_all_wheel_directions_and_motion() {
        let cases = [
            (
                b"\x1b[<64;1;2M",
                Scroll {
                    vertical: 120,
                    horizontal: 0,
                },
            ),
            (
                b"\x1b[<65;1;2M",
                Scroll {
                    vertical: -120,
                    horizontal: 0,
                },
            ),
            (
                b"\x1b[<66;1;2M",
                Scroll {
                    vertical: 0,
                    horizontal: -120,
                },
            ),
            (
                b"\x1b[<67;1;2M",
                Scroll {
                    vertical: 0,
                    horizontal: 120,
                },
            ),
        ];
        for (report, expected) in cases {
            let event = mouse(report).expect("valid wheel report");
            let pointer = pointer(&event);
            assert_eq!(pointer.button, None, "report {report:?}");
            assert_eq!(pointer.scroll, expected, "report {report:?}");
        }

        for report in [&b"\x1b[<32;7;8M"[..], &b"\x1b[<35;7;8m"[..]] {
            let event = mouse(report).expect("valid motion report");
            let pointer = pointer(&event);
            assert_eq!((pointer.x, pointer.y), (7.0, 8.0));
            assert_eq!(pointer.button, None);
            assert_eq!(pointer.scroll, Scroll::default());
        }
    }

    #[test]
    fn mouse_rejects_malformed_reports() {
        let cases: &[&[u8]] = &[
            b"",
            b"\x1b[<",
            b"\x1b[<0",
            b"\x1b[<0;1",
            b"\x1b[<0;1;2",
            b"\x1b[<0;1;2X",
            b"\x1b[<0;1;2;3M",
            b"\x1b[<a;1;2M",
            b"\x1b[<0;a;2M",
            b"\x1b[<0;1;999999999999M",
            b"\x1b[<3;1;2M",
            b"\x1b[<0;1;2M\xff",
        ];
        for report in cases {
            assert!(mouse(report).is_none(), "report {report:?}");
        }
        assert!(mouse(b"\x1b[<0;\xff;2M").is_none());
    }

    #[test]
    fn parser_decodes_complete_stream_in_order() {
        let events = parse(b"\x1b[I\x1b[<0;-10;20M\x1b[97;6u\x1b[200~hi\x1b[201~");
        let [focus, pointer_event, key_event, paste] = events.as_slice() else {
            panic!("expected four events, got {events:?}");
        };
        assert!(matches!(focus, Event::Focus(true)));
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
        assert!(matches!(before.as_slice(), [Event::Focus(true)]));
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
        assert!(matches!(before.as_slice(), [Event::Focus(true)]));
        let [event] = after.as_slice() else {
            panic!("expected one pointer event, got {after:?}");
        };
        let pointer = pointer(event);
        assert_eq!((pointer.x, pointer.y), (-10.0, 20.0));
        assert_eq!(pointer.button, Some((Button::Left, Transition::Pressed)));
    }

    #[test]
    fn parser_keeps_escape_prefix_ambiguity() {
        assert!(matches!(parse(b"\x1b[I").as_slice(), [Event::Focus(true)]));
        assert!(matches!(
            parse(b"\x1b\x1b[I").as_slice(),
            [Event::Focus(true)]
        ));

        let events = parse(b"\x1b[<0;1;2M\x1b[I");
        assert_eq!(events.len(), 2);
        assert_eq!(
            pointer(&events[0]).button,
            Some((Button::Left, Transition::Pressed))
        );
        assert!(matches!(events[1], Event::Focus(true)));

        let events = parse(b"\x1b[<0;1;2X\x1b[I");
        assert!(matches!(&events[..], [Event::Text(text), Event::Focus(true)] if text == "X"));
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
        assert!(matches!(&events[..], [Event::Focus(true)]));
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
        let mut bytes = oversized_mouse(MAX_MUTED);
        bytes.extend_from_slice(b"\x1b[I");
        let (result, events) = parse_result(&bytes);
        let error = result.expect_err("unterminated mouse report must fail");
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(io::Error::from(error).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parser_enforces_the_muted_recovery_deadline() {
        let parser = muted_parser();
        let deadline = parser.muted_deadline.expect("muted after mouse overflow");
        let before = deadline
            .checked_sub(Duration::from_millis(1))
            .expect("deadline has margin");
        assert!(parser.check_recovery(before).is_ok());
        assert_eq!(parser.check_recovery(deadline), Err(InputError));

        let mut recovered = muted_parser();
        recovered.push(b'M').expect("terminated mouse report");
        assert!(recovered.muted_deadline.is_none());
        for &byte in b"\x1b[I" {
            recovered.push(byte).expect("input after recovery");
        }
        assert!(matches!(
            recovered.decoder.events.front(),
            Some(Event::Focus(true))
        ));
        assert_eq!(recovered.decoder.events.len(), 1);
    }

    #[test]
    fn parser_fails_closed_when_a_terminator_arrives_after_the_recovery_deadline() {
        let mut parser = muted_parser();
        parser.muted_deadline = Some(parser.clock.now());
        assert_eq!(parser.push(b'M'), Err(InputError));
        assert!(
            parser.muted_deadline.is_some(),
            "an expired deadline must not be cleared"
        );
        assert!(parser.decoder.events.is_empty());
    }

    #[test]
    fn reader_fails_closed_when_a_read_completes_after_the_recovery_deadline() {
        fn poll_once(
            future: &mut Pin<Box<impl Future<Output = io::Result<()>>>>,
        ) -> Poll<io::Result<()>> {
            let mut context = Context::from_waker(Waker::noop());
            future.as_mut().poll(&mut context)
        }

        let event_loop: calloop::EventLoop<'static, ()> =
            calloop::EventLoop::try_new().expect("event loop");
        let (read, mut write) = UnixStream::pair().expect("socket pair");
        let fd = Descriptor::new(read.into(), &event_loop.handle()).expect("descriptor");
        let mut reader = Reader {
            fd,
            parser: muted_parser(),
            bytes: vec![0; 16 * 1024].into_boxed_slice(),
        };
        let clock = reader.parser.clock.clone();
        let deadline = reader
            .parser
            .muted_deadline
            .expect("muted after mouse overflow");

        let mut read_ready = Box::pin(reader.read_ready());
        assert!(
            poll_once(&mut read_ready).is_pending(),
            "the read must wait for input before the deadline"
        );
        clock.freeze(deadline + RECOVERY_TIMEOUT);
        write.write_all(b"1").expect("write a continuation byte");
        let result = match poll_once(&mut read_ready) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("the read must complete after the continuation byte"),
        };
        let error = result.expect_err("a read completing after the recovery deadline must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(read_ready);
        assert!(reader.parser.muted_deadline.is_some());
        assert!(!reader.pending());
    }

    #[test]
    fn parser_fails_closed_when_oversized_control_never_terminates() {
        let mut bytes = unterminated(b"\x1b]", b'x');
        bytes.extend(std::iter::repeat_n(b'x', MAX_MUTED + 1));
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
}
