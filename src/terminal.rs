//! Terminal input, graphics, and mode management for a pane.

use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute, terminal,
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use vtparse::{CollectingVTActor, CsiParam, VTAction, VTParser};

use crate::{
    Error, Result,
    clipboard::MAX_TEXT,
    kitty::{Presenter, SharedMemory},
    protocol::{self, Hello, Input, PaneToServer, ServerToPane, Show},
    signals,
};

mod clipboard;
mod input;
mod keyboard;
mod output;

use clipboard::{Action as ClipboardAction, Clipboard};
use input::Event;
use keyboard::Keyboard;

const CELL_WIDTH: u16 = 10;
const CELL_HEIGHT: u16 = 20;
/// Cell size assumed when the terminal reports none.
const FALLBACK_CELL: (u16, u16) = (CELL_WIDTH, CELL_HEIGHT);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Quiet period after device attributes, allowing late probe replies.
const PROBE_QUIET: Duration = Duration::from_millis(25);
const PROBE_MAXIMUM: usize = 64 * 1024;
const GRAPHICS_ID: u32 = 31;
const PROBE_QUERY: &[u8] = b"\x1b[16t\x1b[14t\x1b[>q\x1b_Ga=q,f=24,s=1,v=1,i=31;AAAA\x1b\\";
/// Query device attributes last so their reply starts the handshake's quiet
/// window.
const PROBE_QUERY_TAIL: &[u8] = b"\x1b[?1016$p\x1b[?5522$p\x1b[?u\x1b[c";

const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);

/// Terminal capabilities discovered during the handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProbeInfo {
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
    pub graphics: bool,
    pub shared_memory: bool,
    pub sgr_pixels: bool,
    /// OSC 5522 support and the paste-notification mode's previous state.
    pub clipboard: Option<bool>,
    /// Kitty's active enhancements after pushing the requested flags.
    pub keyboard_flags: Option<KeyboardEnhancementFlags>,
    pub name: Option<String>,
}

/// Probe using unbuffered reads: buffered input could hide bytes from `poll`.
fn terminal_probe(
    interrupted: &signals::TerminationFlag,
) -> io::Result<(ProbeInfo, Option<SharedMemory>)> {
    let slot = SharedMemory::new();
    let shared_probe = slot.probe();
    let mut out = io::stdout();
    out.write_all(PROBE_QUERY)?;
    if let Some(command) = &shared_probe {
        out.write_all(command)?;
    }
    out.write_all(PROBE_QUERY_TAIL)?;
    out.flush()?;
    let stdin = io::stdin();
    let mut bytes = Vec::new();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut quiet_deadline = None;
    while !interrupted.interrupted() {
        let now = Instant::now();
        let stop_at = quiet_deadline.unwrap_or(deadline);
        if now >= stop_at {
            break;
        }
        let wait = stop_at - now;
        let mut fds = [PollFd::new(
            &stdin,
            PollFlags::IN | PollFlags::HUP | PollFlags::ERR,
        )];
        let timeout = Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos().into(),
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => return Err(io::Error::from(error)),
        }
        if interrupted.interrupted() {
            break;
        }
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR) {
            break;
        }
        let mut buffer = [0u8; 1024];
        match rustix::io::read(&stdin, &mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&buffer[..read]);
                if quiet_deadline.is_none() && device_attributes_seen(&bytes) {
                    quiet_deadline = Some(Instant::now() + PROBE_QUIET);
                }
                if bytes.len() >= PROBE_MAXIMUM {
                    break;
                }
            }
            Err(Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error)),
        }
    }
    let mut probe = parse_probe_bytes(&bytes);
    probe.shared_memory &= shared_probe.is_some();
    let shared = if probe.shared_memory {
        slot.clear();
        Some(slot)
    } else {
        None
    };
    Ok((probe, shared))
}

/// Match a complete device-attributes reply, not a DECRQM `CSI ?` reply or
/// an incidental `c`, before starting the quiet window.
fn device_attributes_seen(bytes: &[u8]) -> bool {
    probe_actions(bytes).into_iter().any(|action| {
        matches!(action, VTAction::CsiDispatch { params, parameters_truncated: false, byte: b'c' }
            if params.first() == Some(&CsiParam::P(b'?'))
                && params[1..].iter().all(|param| matches!(param, CsiParam::Integer(_) | CsiParam::P(b';'))))
    })
}

fn probe_actions(bytes: &[u8]) -> Vec<VTAction> {
    let mut actor = CollectingVTActor::default();
    VTParser::new().parse(bytes, &mut actor);
    actor.into_vec()
}

fn parse_probe_bytes(bytes: &[u8]) -> ProbeInfo {
    let mut out = ProbeInfo::default();
    let mut name = None;
    for action in probe_actions(bytes) {
        match action {
            VTAction::ApcDispatch(data) => {
                if let Some(body) = data.strip_prefix(b"G") {
                    if let Some(ok) = kitty_reply(body, GRAPHICS_ID) {
                        out.graphics = ok;
                    }
                    if let Some(ok) = kitty_reply(body, crate::kitty::PROBE_ID) {
                        out.shared_memory = ok;
                    }
                }
            }
            VTAction::DcsHook {
                byte: b'|',
                ignored_excess_intermediates: false,
                ..
            } => name = Some(Vec::new()),
            VTAction::DcsPut(byte) => {
                if let Some(name) = &mut name {
                    name.push(byte);
                }
            }
            VTAction::DcsUnhook => {
                if let Some(name) = name.take() {
                    let name = String::from_utf8_lossy(&name).trim().to_owned();
                    if !name.is_empty() {
                        out.name = Some(name);
                    }
                }
            }
            VTAction::CsiDispatch {
                params,
                parameters_truncated: false,
                byte,
            } => probe_csi(&params, byte, &mut out),
            _ => {}
        }
    }
    out
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
                    5522 if matches!(value, 1..=3) => out.clipboard = Some(value != 2),
                    _ => {}
                }
            }
        }
        b'u' if private.is_some() => {
            out.keyboard_flags = u8::try_from(first)
                .ok()
                .map(KeyboardEnhancementFlags::from_bits_truncate);
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

/// Log pane throughput to the user journal once per second.
struct PaneStats {
    window: Instant,
    frames: u32,
    drawn: u32,
    bytes: u64,
    encoded: Duration,
    written: Duration,
    dropped: u32,
}

impl PaneStats {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            frames: 0,
            drawn: 0,
            bytes: 0,
            encoded: Duration::ZERO,
            written: Duration::ZERO,
            dropped: 0,
        }
    }

    fn capabilities(probe: &ProbeInfo, width: u32, height: u32) {
        tracing::info!(
            width, height,
            cell = ?probe.cell_width.zip(probe.cell_height),
            terminal = ?probe.name,
            graphics = probe.graphics,
            shared_memory = probe.shared_memory,
            sgr_pixels = probe.sgr_pixels,
            keyboard_flags = ?probe.keyboard_flags,
            clipboard = ?probe.clipboard,
            "pane attached"
        );
    }

    fn frame(&mut self, bytes: usize, dropped: bool, encoded: Duration, written: Duration) {
        self.frames += 1;
        self.drawn += u32::from(bytes > 0);
        self.dropped += u32::from(dropped);
        self.bytes += bytes as u64;
        self.encoded += encoded;
        self.written += written;
        if self.window.elapsed() < Duration::from_secs(1) {
            return;
        }
        let elapsed = self.window.elapsed().as_secs_f64();
        let frames = std::mem::take(&mut self.frames);
        let drawn = std::mem::take(&mut self.drawn);
        let bytes = std::mem::take(&mut self.bytes);
        let encoded = std::mem::take(&mut self.encoded);
        let written = std::mem::take(&mut self.written);
        let dropped = std::mem::take(&mut self.dropped);
        self.window = Instant::now();
        tracing::info!(
            fps = f64::from(frames) / elapsed,
            drawn,
            dropped,
            unchanged = frames.saturating_sub(drawn).saturating_sub(dropped),
            mb_per_second = bytes as f64 / elapsed / 1e6,
            encode_ms = encoded.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            write_ms = written.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            "pane throughput"
        );
    }
}

/// Attach this terminal to a pane until release or disconnection.
///
/// This invocation exclusively owns terminal input and raw mode. Do not read
/// stdin concurrently; its descriptor is temporarily replaced for nonblocking
/// event parsing and restored when attachment ends.
///
/// # Errors
/// Fails if the terminal cannot be configured or probed, or the pane
/// connection fails. Server-initiated release restores the terminal and
/// returns `Ok`.
#[expect(
    clippy::too_many_lines,
    reason = "terminal setup and the readiness-driven pane event loop"
)]
pub fn attach(socket: &Path, show: Show) -> Result<()> {
    let stream = UnixStream::connect(socket).map_err(|error| {
        Error::io(
            format!(
                "could not reach the pane socket {} (is the server running?)",
                socket.display()
            ),
            error,
        )
    })?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let interrupted = signals::termination_flag()?;
    let mut mode = TerminalGuard::enter()?;
    let (probe, shared) = terminal_probe(&interrupted)?;
    if interrupted.interrupted() {
        mode.restore()?;
        return Ok(());
    }
    if !probe.graphics {
        return Err(Error::GraphicsUnsupported);
    }
    let mut input = input::Reader::new()?;
    if !probe.sgr_pixels {
        return Err(Error::PixelMouseUnsupported);
    }
    if probe
        .keyboard_flags
        .is_none_or(|flags| !flags.contains(KEYBOARD_FLAGS))
    {
        return Err(Error::KeyboardUnsupported);
    }
    mode.enable_mouse()?;
    if let Some(previous) = probe.clipboard {
        mode.enable_clipboard(previous)?;
    }
    let mut cell = probe.cell_width.zip(probe.cell_height);
    let (width, height) = pane_pixels(
        probe.pixel_width.zip(probe.pixel_height),
        window_pixels(),
        (cols, rows),
        cell.unwrap_or(FALLBACK_CELL),
    );
    let hello = Hello {
        width,
        height,
        show,
    };

    let mut tx = stream.try_clone()?;
    protocol::send(&mut tx, &PaneToServer::Hello(hello))?;
    let mut clipboard = Clipboard::new(probe.clipboard.is_some())?;
    let mut keyboard = Keyboard::default();
    let mut presentation = PanePresentation::new(cell, shared);
    PaneStats::capabilities(&probe, width, height);

    let handshake_deadline = Instant::now() + Duration::from_secs(1);
    let mut handshake_done = false;
    let mut reader = protocol::MessageReader::default();
    let mut stdout = output::open()?;
    loop {
        if interrupted.interrupted() {
            mode.restore()?;
            return Ok(());
        }
        if !handshake_done && Instant::now() >= handshake_deadline {
            return Err(Error::PaneHandshakeTimeout);
        }
        if keyboard.deferred_expired() {
            send_deferred(&mut keyboard, &mut tx)?;
        }
        clipboard.poll(&mut presentation.pending);
        send_clipboard_actions(&mut clipboard, &mut keyboard, &mut tx)?;
        let receiving = !presentation.pending.pending();
        let mut fds = [
            PollFd::new(&input.fd, PollFlags::IN | PollFlags::HUP | PollFlags::ERR),
            PollFd::new(
                &stdout,
                PollFlags::HUP
                    | PollFlags::ERR
                    | if presentation.pending.pending() {
                        PollFlags::OUT
                    } else {
                        PollFlags::empty()
                    },
            ),
            PollFd::new(&stream, PollFlags::IN | PollFlags::HUP | PollFlags::ERR),
        ];
        let mut wait = Duration::from_millis(100);
        if !handshake_done {
            wait = wait.min(handshake_deadline.saturating_duration_since(Instant::now()));
        }
        if input.pending() {
            wait = Duration::ZERO;
        }
        let timeout = Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos().into(),
        };
        let watched = if receiving {
            &mut fds[..]
        } else {
            &mut fds[..2]
        };
        match poll(watched, Some(&timeout)) {
            Err(Errno::INTR) => continue,
            Err(error) => return Err(io::Error::from(error).into()),
            Ok(_) => {}
        }
        if interrupted.interrupted() {
            continue;
        }
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR)
            || fds[1].revents().intersects(PollFlags::HUP | PollFlags::ERR)
        {
            return Ok(());
        }
        let writable = fds[1].revents().contains(PollFlags::OUT);
        let input_ready = fds[0].revents().contains(PollFlags::IN);
        let server_ready = receiving
            && fds[2]
                .revents()
                .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
        if server_ready {
            let Ok(message) = reader.try_recv::<ServerToPane>(&stream) else {
                mode.restore_with_message("server disconnected")?;
                return Ok(());
            };
            if let Some(message) = message {
                match message {
                    ServerToPane::HelloOk => {
                        handshake_done = true;
                        clipboard.refresh(false);
                    }
                    ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                        mode.restore_with_message(&reason)?;
                        return Ok(());
                    }
                    ServerToPane::ClipboardWrite(data) => {
                        clipboard.copy(data, &mut presentation.pending);
                    }
                    ServerToPane::ClipboardRead {
                        request,
                        offer,
                        mime,
                    } => clipboard.read(request, offer, mime),
                    ServerToPane::Title(title) => presentation.pending.append(&title_bytes(&title)),
                    ServerToPane::Cursor(shape) => {
                        presentation.pending.append(&cursor_bytes(shape.as_deref()));
                    }
                    ServerToPane::Frame {
                        width,
                        height,
                        y,
                        rgb,
                    } => presentation.draw(width, height, y, rgb, &mut tx)?,
                }
            }
        }
        if writable {
            presentation.drain(&mut stdout, &mut tx)?;
        }
        if input_ready {
            input.read_ready()?;
        }
        for _ in 0..64 {
            if interrupted.interrupted() {
                break;
            }
            let Some(event) = input.next() else {
                break;
            };
            send_event(
                event,
                &mut tx,
                &mut presentation.presenter,
                &mut cell,
                &mut clipboard,
                &mut keyboard,
            )?;
            send_clipboard_actions(&mut clipboard, &mut keyboard, &mut tx)?;
        }
    }
}

/// Own terminal-side presentation buffers together with their accounting.
struct PanePresentation {
    presenter: Presenter,
    stats: PaneStats,
    pending: output::Output,
    frame: Option<PendingFrame>,
}

struct PendingFrame {
    started: Instant,
    encoded: Duration,
    bytes: usize,
    dropped: bool,
}

impl PanePresentation {
    fn new(cell: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            presenter: Presenter::new(cell, shared),
            stats: PaneStats::new(),
            pending: output::Output::default(),
            frame: None,
        }
    }

    fn draw(
        &mut self,
        width: u32,
        height: u32,
        y: u32,
        rgb: Vec<u8>,
        tx: &mut UnixStream,
    ) -> Result<()> {
        let started = Instant::now();
        self.presenter
            .present_into(width, height, y, rgb, self.pending.buffer());
        self.frame = Some(PendingFrame {
            started: Instant::now(),
            encoded: started.elapsed(),
            bytes: self.pending.len(),
            dropped: self.presenter.dropped(),
        });
        self.finish(tx)
    }

    fn drain(&mut self, stdout: &mut impl Write, tx: &mut UnixStream) -> Result<()> {
        self.pending.drain(stdout)?;
        self.finish(tx)
    }

    fn finish(&mut self, tx: &mut UnixStream) -> Result<()> {
        if self.pending.pending() {
            return Ok(());
        }
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        self.stats.frame(
            frame.bytes,
            frame.dropped,
            frame.encoded,
            frame.started.elapsed(),
        );
        protocol::send(
            tx,
            &PaneToServer::Ack {
                drawn: !frame.dropped,
            },
        )?;
        Ok(())
    }
}

fn send_event(
    event: Event,
    tx: &mut UnixStream,
    presenter: &mut Presenter,
    cell: &mut Option<(u16, u16)>,
    clipboard: &mut Clipboard,
    keyboard: &mut Keyboard,
) -> Result<()> {
    match event {
        Event::Key(key) => {
            if let Some(
                input @ Input::Key {
                    code,
                    pressed,
                    modifiers,
                    ..
                },
            ) = keyboard.input(key)
            {
                if pressed && keyboard::binding(code, modifiers) {
                    keyboard.intercept(code);
                }
                let defer = keyboard.deferred()
                    || (pressed
                        && keyboard::paste(code, modifiers)
                        && clipboard.enabled()
                        && clipboard.refresh(true));
                send_input(input, defer, keyboard, tx)?;
            }
        }
        Event::Focus(focused) => {
            clipboard.focus(focused);
            if focused {
                send_input(keyboard.enter(), keyboard.deferred(), keyboard, tx)?;
            } else {
                keyboard.reset();
                protocol::send(tx, &PaneToServer::Input(Input::ResetKeyboard))?;
            }
        }
        Event::Resize => {
            let (cols, rows) = terminal::size().unwrap_or((80, 24));
            let pixels = window_pixels();
            *cell = derived_cell(pixels, (cols, rows)).or(*cell);
            presenter.set_cell_size(*cell);
            let (width, height) =
                pane_pixels(None, pixels, (cols, rows), cell.unwrap_or(FALLBACK_CELL));
            protocol::send(tx, &PaneToServer::Resize { width, height })?;
        }
        Event::Pointer(input) => send_input(input, keyboard.deferred(), keyboard, tx)?,
        Event::Paste(text) => {
            send_deferred(keyboard, tx)?;
            send_paste(tx, text)?;
        }
        Event::Clipboard(bytes) => clipboard.packet(&bytes),
    }
    Ok(())
}

fn send_input(
    input: Input,
    defer: bool,
    keyboard: &mut Keyboard,
    tx: &mut UnixStream,
) -> Result<()> {
    if defer {
        keyboard.defer(input);
        if keyboard.deferred_len() >= 256 {
            tracing::warn!("Clipboard refresh deferred too much input; forwarding original events");
            send_deferred(keyboard, tx)?;
        }
    } else {
        protocol::send(tx, &PaneToServer::Input(input))?;
    }
    Ok(())
}

fn send_clipboard_actions(
    clipboard: &mut Clipboard,
    keyboard: &mut Keyboard,
    tx: &mut UnixStream,
) -> Result<()> {
    while let Some(action) = clipboard.next_action() {
        match action {
            ClipboardAction::Offer {
                offer,
                mimes,
                paste,
            } => {
                if paste && keyboard.deferred() {
                    protocol::send(
                        tx,
                        &PaneToServer::ClipboardOffer {
                            offer,
                            mimes: mimes.clone(),
                            paste: false,
                        },
                    )?;
                    send_deferred(keyboard, tx)?;
                }
                protocol::send(
                    tx,
                    &PaneToServer::ClipboardOffer {
                        offer,
                        mimes,
                        paste,
                    },
                )?;
            }
            ClipboardAction::Reply { request, data } => {
                protocol::send(tx, &PaneToServer::ClipboardReply { request, data })?;
            }
            ClipboardAction::Unblock => send_deferred(keyboard, tx)?,
        }
    }
    Ok(())
}

fn send_deferred(keyboard: &mut Keyboard, tx: &mut UnixStream) -> Result<()> {
    for input in keyboard.take_deferred() {
        protocol::send(tx, &PaneToServer::Input(input))?;
    }
    Ok(())
}

fn send_paste(tx: &mut UnixStream, text: String) -> Result<()> {
    if text.len() <= MAX_TEXT {
        protocol::send(tx, &PaneToServer::Paste(text))?;
    } else {
        tracing::warn!("Ignoring pasted text larger than {MAX_TEXT} bytes");
    }
    Ok(())
}

/// Kernel-reported window pixels; zero dimensions mean unavailable.
fn window_pixels() -> Option<(u32, u32)> {
    terminal::window_size()
        .ok()
        .map(|size| (u32::from(size.width), u32::from(size.height)))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// Prefer terminal-reported pixels, then fresh kernel pixels, then cell
/// geometry.
fn pane_pixels(
    reported: Option<(u32, u32)>,
    fresh: Option<(u32, u32)>,
    cells: (u16, u16),
    cell: (u16, u16),
) -> (u32, u32) {
    reported.or(fresh).unwrap_or_else(|| {
        (
            u32::from(cells.0) * u32::from(cell.0),
            u32::from(cells.1) * u32::from(cell.1),
        )
    })
}

/// Recalculate cell size from window pixels after font zoom.
fn derived_cell(pixels: Option<(u32, u32)>, cells: (u16, u16)) -> Option<(u16, u16)> {
    let (width, height) = pixels?;
    let (cols, rows) = (u32::from(cells.0), u32::from(cells.1));
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell = (
        u16::try_from(width / cols).ok()?,
        u16::try_from(height / rows).ok()?,
    );
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

fn title_bytes(title: &str) -> Vec<u8> {
    let clean = protocol::sanitize_with_limit(title, 512);
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// Set the kitty pointer shape; an empty name restores the terminal default.
fn cursor_bytes(shape: Option<&str>) -> Vec<u8> {
    let name: String = shape
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect();
    format!("\x1b]22;{name}\x1b\\").into_bytes()
}

/// Restores the terminal modes enabled by this pane.
struct TerminalGuard {
    active: bool,
    mouse: bool,
    clipboard: Option<bool>,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self {
            active: true,
            mouse: false,
            clipboard: None,
        };
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            PushKeyboardEnhancementFlags(KEYBOARD_FLAGS),
            event::EnableFocusChange
        )?;
        io::stdout().write_all(b"\x1b[?7l\x1b[?25l\x1b[?2004h")?;
        io::stdout().flush()?;
        Ok(guard)
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        self.mouse = true;
        io::stdout().write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1016h")?;
        io::stdout().flush()?;
        Ok(())
    }

    fn enable_clipboard(&mut self, previous: bool) -> io::Result<()> {
        self.clipboard = Some(previous);
        io::stdout().write_all(b"\x1b[?5522h")?;
        io::stdout().flush()
    }

    fn restore_with_message(&mut self, reason: &str) -> io::Result<()> {
        let clean = protocol::sanitize_with_limit(reason, 512);
        self.restore()?;
        writeln!(io::stdout(), "meowland: {clean}")
    }

    /// Attempt every restoration step even if an earlier one fails; a shell
    /// must not inherit terminal modes after a partial cleanup.
    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut failure = None;
        let mut note = |result: io::Result<()>| {
            if failure.is_none() {
                failure = result.err();
            }
        };
        note(terminal::disable_raw_mode());
        if self.mouse {
            self.mouse = false;
            note(io::stdout().write_all(b"\x1b[?1016l\x1b[?1003l\x1b[?1002l\x1b[?1000l"));
        }
        if let Some(previous) = self.clipboard.take() {
            note(io::stdout().write_all(if previous {
                b"\x1b[?5522h"
            } else {
                b"\x1b[?5522l"
            }));
        }
        note(io::stdout().write_all(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A,q=2;\x1b\\"));
        note(execute!(
            io::stdout(),
            event::DisableFocusChange,
            event::PopKeyboardEnhancementFlags,
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen
        ));
        failure.map_or(Ok(()), Err)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
