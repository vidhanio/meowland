//! Terminal capability probe and reply decoding.

use std::{
    io::Write as _,
    time::{Duration, Instant},
};

use calloop::LoopHandle;
use futures::FutureExt;
use meowland_async::{
    asynchronous::{Descriptor, sleep_until, when},
    signals,
};
use meowland_core::pixels::FrameSize;
use meowland_input as input;
use meowland_kitty::{query_frame_edits, query_shared};
use meowland_protocol::SharedObject;
use vtparse::{CollectingVTActor, CsiParam, VTAction, VTParser};

use super::output;

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Quiet period after device attributes, allowing late probe replies.
const PROBE_QUIET: Duration = Duration::from_millis(25);
const PROBE_MAXIMUM: usize = 64 * 1024;
const FRAME_EDIT_ID: u32 = 30;
const SHARED_ID: u32 = 32;
const PROBE_QUERY: &[u8] = b"\x1b[16t\x1b[14t\x1b[>q";
/// Query device attributes last so their reply starts the handshake's quiet
/// window.
const PROBE_QUERY_TAIL: &[u8] = b"\x1b[?1016$p\x1b[?5522$p\x1b[?u\x1b[c";

/// Terminal capabilities discovered during the handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProbeInfo {
    /// Pane size in physical pixels, if the terminal reported it.
    pub pixels: Option<(u32, u32)>,
    /// Cell size in physical pixels, if the terminal reported it.
    pub cell: Option<(u16, u16)>,
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
            cell = ?self.cell,
            pixels = ?self.pixels,
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
    pending.append(&query_frame_edits(FRAME_EDIT_ID));
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
                    4 => out.pixels = Some((width, height)),
                    6 => out.cell = u16::try_from(width).ok().zip(u16::try_from(height).ok()),
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

#[cfg(test)]
mod tests {
    use crossterm::event::KeyboardEnhancementFlags;

    use super::*;

    fn parse_chunks(chunks: &[&[u8]]) -> ProbeInfo {
        let mut parser = ProbeParser::default();
        for chunk in chunks {
            parser.parse(chunk);
        }
        parser.finish()
    }

    fn parse(bytes: &[u8]) -> ProbeInfo {
        parse_chunks(&[bytes])
    }

    fn attributes_seen(chunks: &[&[u8]]) -> bool {
        let mut parser = ProbeParser::default();
        for chunk in chunks {
            parser.parse(chunk);
        }
        parser.attributes_seen()
    }

    #[test]
    fn probe_decodes_pixel_and_cell_sizes_across_chunks() {
        let info = parse_chunks(&[b"\x1b[4;3", b"60;640t\x1b[6;20", b";10t"]);
        assert_eq!(info.pixels, Some((640, 360)));
        assert_eq!(info.cell, Some((10, 20)));
    }

    #[test]
    fn probe_ignores_invalid_sizes() {
        for report in [
            &b"\x1b[4;0;640t"[..],
            &b"\x1b[4;360;0t"[..],
            &b"\x1b[4;360t"[..],
            &b"\x1b[4;360;640"[..],
        ] {
            assert_eq!(parse(report).pixels, None, "report {report:?}");
        }
        for report in [
            &b"\x1b[6;20;70000t"[..],
            &b"\x1b[6;70000;20t"[..],
            &b"\x1b[6;20;0t"[..],
        ] {
            assert_eq!(parse(report).cell, None, "report {report:?}");
        }
    }

    #[test]
    fn probe_starts_quiet_window_only_after_device_attributes() {
        assert!(attributes_seen(&[b"\x1b[?1;2c"]));
        assert!(attributes_seen(&[b"\x1b[?1", b";2c"]));
        assert!(!attributes_seen(&[b"\x1b[c"]));
        assert!(!attributes_seen(&[b"\x1b[?1016;1$y"]));
        assert!(attributes_seen(&[b"\x1b[?1016;1$y\x1b[?1;2c"]));
    }

    #[test]
    fn probe_decodes_kitty_graphics_replies_by_id() {
        let info = parse(b"\x1b_Gi=30;OK\x1b\\");
        assert!(info.frame_edits);
        assert!(!info.shared_memory);

        let info = parse(b"\x1b_Gi=32;OK\x1b\\");
        assert!(!info.frame_edits);
        assert!(info.shared_memory);

        let info = parse(b"\x1b_Gi=30;ENOENT\x1b\\");
        assert!(!info.frame_edits);

        let info = parse(b"\x1b_Gi=31;OK\x1b\\");
        assert!(!info.frame_edits);
        assert!(!info.shared_memory);

        let info = parse(b"\x1b_Gx=1,i=30;OK\x1b\\");
        assert!(info.frame_edits);
    }

    #[test]
    fn probe_decodes_decrqm_reports() {
        for value in [1, 3] {
            let report = format!("\x1b[?1016;{value}$y");
            assert!(parse(report.as_bytes()).sgr_pixels, "value {value}");
        }
        for value in [0, 4] {
            let report = format!("\x1b[?1016;{value}$y");
            assert!(!parse(report.as_bytes()).sgr_pixels, "value {value}");
        }

        let cases = [
            (1, Some(super::super::terminal::ClipboardMode::Enabled)),
            (2, Some(super::super::terminal::ClipboardMode::Disabled)),
            (3, Some(super::super::terminal::ClipboardMode::Enabled)),
            (0, None),
            (4, None),
        ];
        for (value, expected) in cases {
            let report = format!("\x1b[?5522;{value}$y");
            assert_eq!(
                parse(report.as_bytes()).clipboard,
                expected,
                "value {value}"
            );
        }
    }

    #[test]
    fn probe_decodes_keyboard_enhancement_flags() {
        let info = parse(b"\x1b[?5u");
        assert_eq!(
            info.keyboard_flags,
            Some(KeyboardEnhancementFlags::from_bits_retain(5))
        );

        let info = parse(b"\x1b[?0u");
        assert_eq!(info.keyboard_flags, Some(KeyboardEnhancementFlags::empty()));
    }

    #[test]
    fn probe_decodes_xtversion_names() {
        let info = parse(b"\x1bP>|kitty(0.40.1)\x1b\\");
        assert_eq!(info.name.as_deref(), Some("kitty(0.40.1)"));

        let info = parse(b"\x1bP>|  WezTerm  \x1b\\");
        assert_eq!(info.name.as_deref(), Some("WezTerm"));

        let info = parse(b"\x1bP>|\x1b\\");
        assert_eq!(info.name, None);
    }
}
