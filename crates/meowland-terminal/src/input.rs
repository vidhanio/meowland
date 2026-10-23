//! Nonblocking terminal input with signed SGR pixel mouse decoding and
//! `vtparse`.

use std::{
    collections::VecDeque,
    io,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use meowland_protocol::{Input, clipboard::MAX_TEXT};
use rustix::{
    fs::{Mode, OFlags, open},
    io::Errno,
};
use signal_hook::SigId;
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
    pub(super) code: u32,
    pub(super) modifiers: u8,
    pub(super) kind: Kind,
}

pub enum Event {
    Key(Key),
    Pointer(Input),
    Focus(bool),
    Resize,
    Paste(String),
    Clipboard(Vec<u8>),
    ClipboardText(Vec<u8>),
}

pub struct Reader {
    pub(super) fd: OwnedFd,
    parser: Parser,
    resized: Arc<AtomicBool>,
    handler: SigId,
}

impl Reader {
    pub(super) fn new() -> io::Result<Self> {
        let fd = open(
            "/proc/self/fd/0",
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        let resized = Arc::new(AtomicBool::new(false));
        let handler =
            signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&resized))?;
        Ok(Self {
            fd,
            parser: Parser::new(),
            resized,
            handler,
        })
    }

    pub(super) fn pending(&self) -> bool {
        !self.parser.decoder.events.is_empty() || self.resized.load(Ordering::Relaxed)
    }

    pub(super) fn next(&mut self) -> Option<Event> {
        if self.resized.swap(false, Ordering::Relaxed) {
            return Some(Event::Resize);
        }
        self.parser.decoder.events.pop_front()
    }

    pub(super) fn read_ready(&mut self) -> io::Result<()> {
        if !self.parser.decoder.events.is_empty() {
            return Ok(());
        }
        let mut bytes = [0; 16 * 1024];
        let read = match rustix::io::read(&self.fd, &mut bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => read,
            Err(Errno::AGAIN | Errno::INTR) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for &byte in &bytes[..read] {
            self.parser.push(byte);
        }
        Ok(())
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

    fn print(&mut self, _character: char) {}

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
    Some(Event::Key(Key {
        code,
        modifiers,
        kind,
    }))
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
