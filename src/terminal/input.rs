//! Nonblocking terminal input with `vtparse`'s streaming escape parser.

use std::{
    collections::VecDeque,
    io,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use rustix::{
    fs::{Mode, OFlags, open},
    io::Errno,
};
use signal_hook::SigId;
use vtparse::{CsiParam, VTActor, VTParser};

use crate::{clipboard::MAX_TEXT, protocol::Input};

const MAX_CONTROL: usize = 64 * 1024;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Key {
    pub(super) code: u32,
    pub(super) modifiers: u8,
    pub(super) kind: Kind,
}

pub(super) enum Event {
    Key(Key),
    Pointer(Input),
    Focus(bool),
    Resize,
    Paste(String),
    Clipboard(Vec<u8>),
}

pub(super) struct Reader {
    pub(super) fd: OwnedFd,
    parser: VTParser,
    decoder: Decoder,
    control_bytes: usize,
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
            parser: VTParser::new(),
            decoder: Decoder::default(),
            control_bytes: 0,
            resized,
            handler,
        })
    }

    pub(super) fn pending(&self) -> bool {
        !self.decoder.events.is_empty() || self.resized.load(Ordering::Relaxed)
    }

    pub(super) fn next(&mut self) -> Option<Event> {
        if self.resized.swap(false, Ordering::Relaxed) {
            return Some(Event::Resize);
        }
        self.decoder.events.pop_front()
    }

    pub(super) fn read_ready(&mut self) -> io::Result<()> {
        if !self.decoder.events.is_empty() {
            return Ok(());
        }
        let mut bytes = [0; 16 * 1024];
        let read = match rustix::io::read(&self.fd, &mut bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => read,
            Err(Errno::AGAIN | Errno::INTR) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for byte in &bytes[..read] {
            if self.decoder.paste.is_some() {
                self.decoder.paste_byte(*byte);
                continue;
            }
            self.parser.parse_byte(*byte, &mut self.decoder);
            self.control_bytes = if self.parser.is_ground() {
                0
            } else {
                self.control_bytes + 1
            };
            if self.control_bytes > MAX_CONTROL {
                self.parser = VTParser::new();
                self.control_bytes = 0;
                tracing::warn!("Discarded oversized terminal control sequence");
            }
        }
        Ok(())
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.handler);
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

pub(super) fn number(params: &[CsiParam]) -> Option<u32> {
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
    if matches!(final_byte, b'M' | b'm') {
        return mouse(params, final_byte);
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

fn mouse(params: &[CsiParam], final_byte: u8) -> Option<Event> {
    let mut fields = params
        .strip_prefix(&[CsiParam::P(b'<')])?
        .split(|param| *param == CsiParam::P(b';'));
    let code = fields.next().and_then(number)?;
    let x = fields.next().and_then(number)?.checked_sub(1)?;
    let y = fields.next().and_then(number)?.checked_sub(1)?;
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
