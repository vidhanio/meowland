//! Nonblocking terminal input with signed SGR pixel mouse decoding and
//! `vtparse`.

use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use meowland_core::asynchronous::Descriptor;
use meowland_protocol::{
    Input,
    clipboard::MAX_TEXT,
    modifiers::{ALT, CONTROL, HYPER, META, SUPER},
};
use rustix::fs::{Mode, OFlags, open};
use signal_hook::SigId;
use tokio::io::AsyncReadExt;
use vtparse::{CsiParam, VTActor, VTParser};

const MAX_CONTROL: usize = MAX_TEXT.div_ceil(3) * 4 + 64;
const MOUSE_PREFIX: &[u8] = b"\x1b[<";
const MAX_MOUSE: usize = 64;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Copy, Debug)]
pub struct Key {
    pub code: u32,
    pub modifiers: u8,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum Event {
    Key(Key),
    Text(String),
    Pointer(Input),
    Focus(bool),
    Resize,
    Paste(String),
    Clipboard(Vec<u8>),
    ClipboardText(Vec<u8>),
}

pub struct Reader {
    fd: Descriptor,
    parser: Parser,
    bytes: Box<[u8]>,
    resized: Arc<AtomicBool>,
    handler: SigId,
}

impl Reader {
    /// Open terminal input without changing stdin's blocking flags.
    ///
    /// # Errors
    /// Returns an error if opening, registering, or watching input fails.
    pub fn new() -> io::Result<Self> {
        let fd = open(
            "/proc/self/fd/0",
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        let fd = Descriptor::new(fd)?;
        let resized = Arc::new(AtomicBool::new(false));
        let handler =
            signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&resized))?;
        Ok(Self {
            fd,
            parser: Parser::new(),
            bytes: vec![0; 16 * 1024].into_boxed_slice(),
            resized,
            handler,
        })
    }

    #[must_use]
    pub fn pending(&self) -> bool {
        !self.parser.decoder.events.is_empty() || self.resized.load(Ordering::Relaxed)
    }

    /// Pop a buffered event, prioritizing a pending resize.
    pub fn next_event(&mut self) -> Option<Event> {
        if self.resized.swap(false, Ordering::Relaxed) {
            return Some(Event::Resize);
        }
        self.parser.decoder.events.pop_front()
    }

    /// Read and decode the next bounded input chunk.
    ///
    /// # Errors
    /// Returns an error on EOF or input failure.
    pub async fn read_ready(&mut self) -> io::Result<()> {
        if !self.parser.decoder.events.is_empty() {
            return Ok(());
        }
        let read = self.fd.read(&mut self.bytes).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        for &byte in &self.bytes[..read] {
            self.parser.push(byte);
        }
        Ok(())
    }

    /// Read raw capability replies before starting event decoding.
    ///
    /// # Errors
    /// Returns an error if terminal input fails.
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

impl Drop for Reader {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.handler);
    }
}

/// Decode signed mouse coordinates before the VT parser rejects their minus
/// signs.
struct Parser {
    terminal: VTParser,
    decoder: Decoder,
    mouse: Vec<u8>,
    control_bytes: usize,
}

impl Parser {
    fn new() -> Self {
        Self {
            terminal: VTParser::new(),
            decoder: Decoder::default(),
            mouse: Vec::new(),
            control_bytes: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.decoder.paste.is_some() {
            self.decoder.paste_byte(byte);
            return;
        }
        if self.mouse.starts_with(MOUSE_PREFIX) {
            match byte {
                b'M' | b'm' => {
                    if self.mouse.len() < MAX_MOUSE {
                        self.mouse.push(byte);
                        if let Some(event) = mouse(&self.mouse) {
                            self.decoder.events.push_back(event);
                        }
                    }
                    self.mouse.clear();
                }
                b'0'..=b'9' | b';' | b'-' => {
                    if self.mouse.len() < MAX_MOUSE {
                        self.mouse.push(byte);
                    }
                }
                _ => {
                    self.mouse.clear();
                    self.push(byte);
                }
            }
            return;
        }
        if !self.mouse.is_empty() {
            if byte == b'\x1b' {
                self.mouse.truncate(1);
                return;
            }
            self.mouse.push(byte);
            if MOUSE_PREFIX.starts_with(&self.mouse) {
                return;
            }
            let mut pending = std::mem::take(&mut self.mouse);
            for &byte in &pending {
                self.terminal_byte(byte);
            }
            pending.clear();
            self.mouse = pending;
            return;
        }
        if self.terminal.is_ground() && byte == b'\x1b' {
            self.mouse.push(byte);
        } else {
            self.terminal_byte(byte);
        }
    }

    fn terminal_byte(&mut self, byte: u8) {
        self.terminal.parse_byte(byte, &mut self.decoder);
        self.control_bytes = if self.terminal.is_ground() {
            0
        } else {
            self.control_bytes + 1
        };
        if self.control_bytes > MAX_CONTROL {
            self.terminal = VTParser::new();
            self.control_bytes = 0;
            tracing::warn!("Discarded oversized terminal control sequence");
        }
    }
}

#[derive(Default)]
struct Decoder {
    events: VecDeque<Event>,
    paste: Option<Vec<u8>>,
    oversized_paste: bool,
}

impl Decoder {
    fn paste_byte(&mut self, byte: u8) {
        let paste = self.paste.as_mut().unwrap();
        paste.push(byte);
        if paste.ends_with(PASTE_END) {
            paste.truncate(paste.len() - PASTE_END.len());
            let paste = self.paste.take().unwrap();
            if !self.oversized_paste
                && paste.len() <= MAX_TEXT
                && let Ok(text) = String::from_utf8(paste)
            {
                self.events.push_back(Event::Paste(text));
            }
            self.oversized_paste = false;
        } else if self.oversized_paste || paste.len() > MAX_TEXT + PASTE_END.len() {
            self.oversized_paste = true;
            paste.drain(..paste.len().saturating_sub(PASTE_END.len() - 1));
        }
    }
}

impl VTActor for Decoder {
    fn csi_dispatch(&mut self, params: &[CsiParam], truncated: bool, byte: u8) {
        if truncated {
            return;
        }
        if params == [CsiParam::Integer(200)] && byte == b'~' {
            self.paste = Some(Vec::new());
        } else if let Some(event) = csi(params, byte) {
            self.events.push_back(event);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
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
        if !character.is_control() && crate::keyboard::key_code(u32::from(character)).is_none() {
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
    let modifiers = (bits & !6) | ((bits & 2) << 1) | ((bits & 4) >> 1);
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
    let shortcut = modifiers & (CONTROL | ALT | SUPER | HYPER | META) != 0;
    if let Some(text) = text
        && matches!(kind, Kind::Press | Kind::Repeat)
        && !shortcut
        && crate::keyboard::key_code(code).is_none()
    {
        return Some(Event::Text(text));
    }
    Some(Event::Key(Key {
        code,
        modifiers,
        kind,
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
    (!text.is_empty() && text.len() <= MAX_TEXT).then_some(text)
}

fn mouse(report: &[u8]) -> Option<Event> {
    let (&final_byte, fields) = report.strip_prefix(MOUSE_PREFIX)?.split_last()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let mut fields = std::str::from_utf8(fields).ok()?.split(';');
    let code: u32 = fields.next()?.parse().ok()?;
    let x: i32 = fields.next()?.parse().ok()?;
    let y: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let (button, pressed, scroll) = if code & 64 != 0 {
        (
            None,
            false,
            match code & 3 {
                0 => 120,
                1 => -120,
                _ => return None,
            },
        )
    } else if code & (32 | 128) != 0 {
        (None, false, 0)
    } else {
        (
            match code & 3 {
                0 => Some(0),
                1 => Some(2),
                2 => Some(1),
                _ => None,
            },
            final_byte == b'M',
            0,
        )
    };
    Some(Event::Pointer(Input::Pointer {
        x: f64::from(x),
        y: f64::from(y),
        button,
        pressed,
        scroll,
    }))
}
