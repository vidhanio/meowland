//! Terminal capability probe and reply decoding.

use std::{
    io::Write as _,
    time::{Duration, Instant},
};

use calloop::LoopHandle;
use futures::FutureExt;
use meowland_core::{
    asynchronous::{Descriptor, sleep_until, when},
    pixels::FrameSize,
    signals,
};
use meowland_input as input;
use meowland_kitty::query_shared;
use meowland_protocol::SharedObject;
use vtparse::{CollectingVTActor, CsiParam, VTAction, VTParser};

use super::output;

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Quiet period after device attributes, allowing late probe replies.
const PROBE_QUIET: Duration = Duration::from_millis(25);
const PROBE_MAXIMUM: usize = 64 * 1024;
const FRAME_EDIT_ID: u32 = 30;
const SHARED_ID: u32 = 32;
const FRAME_EDIT_QUERY: &[u8] = b"\x1b_Ga=t,f=24,s=1,v=1,i=30,q=2;AAAA\x1b\\\x1b_Ga=f,f=24,s=1,v=1,i=30,r=1;AAAA\x1b\\\x1b_Ga=d,d=I,i=30,q=2;\x1b\\";
const PROBE_QUERY: &[u8] = b"\x1b[16t\x1b[14t\x1b[>q";
/// Query device attributes last so their reply starts the handshake's quiet
/// window.
const PROBE_QUERY_TAIL: &[u8] = b"\x1b[?1016$p\x1b[?5522$p\x1b[?u\x1b[c";

/// Terminal capabilities discovered during the handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProbeInfo {
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
    pub frame_edits: bool,
    pub shared_memory: bool,
    pub sgr_pixels: bool,
    /// OSC 5522 support and the paste-notification mode's previous state.
    pub clipboard: Option<super::terminal::ClipboardMode>,
    /// Kitty's active enhancements after pushing the requested flags.
    pub keyboard_flags: Option<crossterm::event::KeyboardEnhancementFlags>,
    pub name: Option<String>,
}

impl ProbeInfo {
    pub(crate) fn log_capabilities(&self, width: u32, height: u32) {
        tracing::info!(
            width, height,
            cell = ?self.cell_width.zip(self.cell_height),
            terminal = ?self.name,
            frame_edits = self.frame_edits,
            shared_memory = self.shared_memory,
            sgr_pixels = self.sgr_pixels,
            keyboard_flags = ?self.keyboard_flags,
            clipboard = ?self.clipboard,
            "pane attached"
        );
    }
}

/// Probe through the presentation descriptors, returning `None` on termination.
#[expect(
    clippy::future_not_send,
    reason = "terminal probing runs on calloop's local executor"
)]
pub(crate) async fn terminal_probe(
    input: &mut input::Reader,
    output: &mut Descriptor,
    termination: &mut signals::Termination,
    handle: &LoopHandle<'static, ()>,
) -> std::io::Result<Option<ProbeInfo>> {
    let shared_probe = shared_probe();
    let mut pending = output::Output::default();
    pending.append(PROBE_QUERY);
    pending.append(FRAME_EDIT_QUERY);
    if let Some((_, command)) = &shared_probe {
        pending.append(command);
    }
    pending.append(PROBE_QUERY_TAIL);
    let mut parser = ProbeParser::default();
    let mut received = 0;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut quiet_deadline = None;
    loop {
        let mut buffer = [0; 1024];
        let writing = pending.pending();
        futures::select! {
            _ = termination.recv().fuse() => return Ok(None),
            result = sleep_until(handle, Some(quiet_deadline.unwrap_or(deadline))).fuse() => { result?; break; },
            result = when(writing.then(|| pending.drain(output))).fuse() => result?,
            result = when((!writing).then(|| input.read_raw(&mut buffer))).fuse() => {
                let read = result?;
                if read == 0 { break; }
                parser.parse(&buffer[..read]);
                received += read;
                if quiet_deadline.is_none() && parser.attributes_seen() {
                    quiet_deadline = Some(Instant::now() + PROBE_QUIET);
                }
                if received >= PROBE_MAXIMUM { break; }
            }
        }
    }
    let mut probe = parser.finish();
    probe.shared_memory &= shared_probe.is_some();
    Ok(Some(probe))
}

fn shared_probe() -> Option<(SharedObject, Vec<u8>)> {
    let (object, mut file) = SharedObject::create(FrameSize::new(1, 1)?).ok()?;
    file.write_all(&[0; 3]).ok()?;
    let command = query_shared(object.frame(), SHARED_ID);
    Some((object, command))
}

struct ProbeParser {
    terminal: VTParser,
    info: ProbeInfo,
    name: Option<Vec<u8>>,
    attributes: bool,
}

impl Default for ProbeParser {
    fn default() -> Self {
        Self {
            terminal: VTParser::new(),
            info: ProbeInfo::default(),
            name: None,
            attributes: false,
        }
    }
}

impl ProbeParser {
    fn parse(&mut self, bytes: &[u8]) {
        let mut actor = CollectingVTActor::default();
        self.terminal.parse(bytes, &mut actor);
        for action in actor {
            self.apply(action);
        }
    }

    const fn attributes_seen(&self) -> bool {
        self.attributes
    }

    fn finish(self) -> ProbeInfo {
        self.info
    }

    fn apply(&mut self, action: VTAction) {
        match action {
            VTAction::ApcDispatch(data) => {
                if let Some(body) = data.strip_prefix(b"G") {
                    if let Some(ok) = kitty_reply(body, FRAME_EDIT_ID) {
                        self.info.frame_edits = ok;
                    }
                    if let Some(ok) = kitty_reply(body, SHARED_ID) {
                        self.info.shared_memory = ok;
                    }
                }
            }
            VTAction::DcsHook {
                byte: b'|',
                ignored_excess_intermediates: false,
                ..
            } => self.name = Some(Vec::new()),
            VTAction::DcsPut(byte) => {
                if let Some(name) = &mut self.name {
                    name.push(byte);
                }
            }
            VTAction::DcsUnhook => {
                if let Some(name) = self.name.take() {
                    let name = String::from_utf8_lossy(&name).trim().to_owned();
                    if !name.is_empty() {
                        self.info.name = Some(name);
                    }
                }
            }
            VTAction::CsiDispatch {
                params,
                parameters_truncated: false,
                byte,
            } => {
                self.attributes |= device_attributes(&params, byte);
                probe_csi(&params, byte, &mut self.info);
            }
            _ => {}
        }
    }
}

/// Match a complete device-attributes reply, not a DECRQM `CSI ?` reply or
/// an incidental `c`, before starting the quiet window.
fn device_attributes(params: &[CsiParam], byte: u8) -> bool {
    byte == b'c'
        && params.first() == Some(&CsiParam::P(b'?'))
        && params[1..]
            .iter()
            .all(|param| matches!(param, CsiParam::Integer(_) | CsiParam::P(b';')))
}

fn probe_csi(params: &[CsiParam], byte: u8, out: &mut ProbeInfo) {
    let private = params.strip_prefix(&[CsiParam::P(b'?')]);
    let params = private.unwrap_or(params);
    let params = params.strip_suffix(&[CsiParam::P(b'$')]).unwrap_or(params);
    let mut fields = params
        .split(|param| *param == CsiParam::P(b';'))
        .map(input::number);
    let Some(first) = fields.next().flatten() else {
        return;
    };
    match byte {
        b't' => {
            if let (Some(height), Some(width)) = (fields.next().flatten(), fields.next().flatten())
                && height != 0
                && width != 0
            {
                match first {
                    4 => {
                        out.pixel_height = Some(height);
                        out.pixel_width = Some(width);
                    }
                    6 => {
                        out.cell_height = u16::try_from(height).ok();
                        out.cell_width = u16::try_from(width).ok();
                    }
                    _ => {}
                }
            }
        }
        b'y' if private.is_some() => {
            if let Some(value) = fields.next().flatten() {
                match first {
                    1016 => out.sgr_pixels = matches!(value, 1..=3),
                    5522 if matches!(value, 1..=3) => {
                        out.clipboard = Some(if value == 2 {
                            super::terminal::ClipboardMode::Disabled
                        } else {
                            super::terminal::ClipboardMode::Enabled
                        });
                    }
                    _ => {}
                }
            }
        }
        b'u' if private.is_some() => {
            out.keyboard_flags = u8::try_from(first)
                .ok()
                .map(crossterm::event::KeyboardEnhancementFlags::from_bits_retain);
        }
        _ => {}
    }
}

/// Kitty graphics reply: `i=<id> ; <message>`.
fn kitty_reply(body: &[u8], id: u32) -> Option<bool> {
    let (control, payload) = body
        .iter()
        .position(|byte| *byte == b';')
        .map_or((body, &[][..]), |index| {
            (&body[..index], &body[index + 1..])
        });
    let for_us = control
        .split(|byte| *byte == b',')
        .any(|field| field.strip_prefix(b"i=").and_then(parse_u32) == Some(id));
    for_us.then(|| payload.starts_with(b"OK"))
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}
