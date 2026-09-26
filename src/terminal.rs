//! Terminal side of a meowland pane.
//!
//! The terminal is deliberately kept in this module: the server never needs
//! to know about raw mode, terminal escape sequences, or crossterm events.

use std::{
    fs,
    io::{self, Write},
    os::unix::net::UnixStream,
    path::Path,
    process,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind},
    execute, terminal,
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};

use crate::{
    Error, Result, diag,
    kitty::{Presenter, SharedMemory},
    protocol::{self, Hello, Input, PaneToServer, ServerToPane, Show},
};

const CELL_WIDTH: u16 = 10;
const CELL_HEIGHT: u16 = 20;
/// The size of a cell a pane assumes when the terminal never reported one, so
/// that a pane in a quiet terminal still has a size to send.
const FALLBACK_CELL: (u16, u16) = (CELL_WIDTH, CELL_HEIGHT);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// After the last reply, a terminal gets this long to send stragglers.
const PROBE_QUIET: Duration = Duration::from_millis(25);
const PROBE_MAXIMUM: usize = 64 * 1024;
const GRAPHICS_ID: u32 = 31;

/// Everything a pane asks the terminal after it takes it over.  Device
/// attributes are asked for last: they are the one reply every terminal gives,
/// so the reply is the end of the handshake.
const PROBE_QUERY: &[u8] = b"\x1b[16t\x1b[14t\x1b[>q\x1b_Ga=q,f=24,s=1,v=1,i=31;AAAA\x1b\\";
const PROBE_QUERY_TAIL: &[u8] = b"\x1b[?1016$p\x1b[c";

/// What a terminal said about itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProbeInfo {
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
    /// The graphics query was answered with `OK`.
    pub graphics: bool,
    /// The one-pixel shared memory query was answered with `OK`.
    pub shared_memory: bool,
    /// `Some(true)` when the terminal reports `SGR-Pixels` mouse mode.
    pub sgr_pixels: Option<bool>,
    /// The name from an `XTVERSION` reply, if the terminal sent one.
    pub name: Option<String>,
}

/// The coordinates a terminal reports with mouse events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseUnits {
    Pixels,
    Cells,
}

/// Ask the terminal about itself and read the replies.  The reads are raw, so
/// they stay in step with the poll that says there is something to read; going
/// through a buffered reader would leave bytes invisible to the next poll.
fn terminal_probe() -> io::Result<(ProbeInfo, Option<SharedMemory>)> {
    let slot = SharedMemory::new();
    let shared_probe = slot.probe();
    let mut query = Vec::from(PROBE_QUERY);
    query.extend(shared_probe.iter().flatten().copied());
    query.extend_from_slice(PROBE_QUERY_TAIL);
    let mut out = io::stdout();
    out.write_all(&query)?;
    out.flush()?;
    let stdin = io::stdin();
    let mut bytes = Vec::new();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut quiet_deadline = None;
    loop {
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
        if poll(&mut fds, Some(&timeout))? == 0 {
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
    // A terminal that answered `OK` has read, and so unlinked, the probe
    // object; clear it again for the ones that answer without reading.
    let shared = if probe.shared_memory {
        slot.clear();
        Some(slot)
    } else {
        None
    };
    Ok((probe, shared))
}

/// Whether a device-attributes reply is in `bytes`.  That reply is what ends
/// the probe's quiet window, and the DECRQM reply is a `CSI ?` sequence too,
/// so the whole `CSI ? <params> c` shape is matched rather than a stray `c`.
fn device_attributes_seen(bytes: &[u8]) -> bool {
    let mut index = 0;
    while let Some(offset) = find(&bytes[index..], b"\x1b[?") {
        let start = index + offset;
        index = start + 3;
        let Some((body, final_byte, _)) = control_sequence(bytes, start) else {
            return false;
        };
        if final_byte == b'c'
            && body[1..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b';')
        {
            return true;
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse the terminal replies.  Keeping this separate from the read loop makes
/// it testable against captured handshakes.
fn parse_probe_bytes(bytes: &[u8]) -> ProbeInfo {
    let mut out = ProbeInfo::default();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        match bytes.get(index + 1) {
            Some(b'_') => {
                let rest = &bytes[index + 2..];
                let Some(end) = find(rest, b"\x1b\\") else {
                    break;
                };
                if let Some(body) = rest[..end].strip_prefix(b"G") {
                    if let Some(ok) = kitty_reply(body, GRAPHICS_ID) {
                        out.graphics = ok;
                    }
                    if let Some(ok) = kitty_reply(body, crate::kitty::PROBE_ID) {
                        out.shared_memory = ok;
                    }
                }
                index += 2 + end + 2;
            }
            Some(b'P') => {
                let rest = &bytes[index + 2..];
                let Some(end) = find(rest, b"\x1b\\") else {
                    break;
                };
                if let Some(name) = rest[..end].strip_prefix(b">|") {
                    let name = String::from_utf8_lossy(name).trim().to_owned();
                    if !name.is_empty() {
                        out.name = Some(name);
                    }
                }
                index += 2 + end + 2;
            }
            Some(b'[') => {
                let Some((body, final_byte, next)) = control_sequence(bytes, index) else {
                    break;
                };
                match final_byte {
                    b't' => parse_size_reply(body, &mut out),
                    b'y' => parse_decrqm_reply(body, &mut out),
                    _ => {}
                }
                index = next;
            }
            _ => index += 1,
        }
    }
    out
}

/// Split a control sequence at `start` into its body and final byte.
fn control_sequence(bytes: &[u8], start: usize) -> Option<(&[u8], u8, usize)> {
    let body_start = start + 2;
    let mut index = body_start;
    while index < bytes.len() {
        let byte = bytes[index];
        if (0x40..=0x7e).contains(&byte) {
            return Some((&bytes[body_start..index], byte, index + 1));
        }
        index += 1;
    }
    None
}

/// `CSI 4 ; height ; width t` is the text area, `CSI 6 ; height ; width t` one
/// cell.  Zero is how terminals answer "no idea", and is treated as no answer.
fn parse_size_reply(body: &[u8], out: &mut ProbeInfo) {
    let body = body.strip_prefix(b"?").unwrap_or(body);
    let mut fields = body.split(|byte| *byte == b';');
    let (Some(kind), Some(height), Some(width)) = (
        fields.next().and_then(parse_u32),
        fields.next().and_then(parse_u32),
        fields.next().and_then(parse_u32),
    ) else {
        return;
    };
    if height == 0 || width == 0 {
        return;
    }
    match kind {
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

/// `CSI ? mode ; value $ y`: zero means unsupported, anything else means the
/// terminal knows the mode.
fn parse_decrqm_reply(body: &[u8], out: &mut ProbeInfo) {
    let Some(body) = body.strip_prefix(b"?") else {
        return;
    };
    let body = body.strip_suffix(b"$").unwrap_or(body);
    let mut fields = body.split(|byte| *byte == b';');
    let (Some(mode), Some(value)) = (
        fields.next().and_then(parse_u32),
        fields.next().and_then(parse_u32),
    ) else {
        return;
    };
    if mode == 1016 {
        out.sgr_pixels = Some(value != 0);
    }
}

/// A kitty graphics reply: `i=<id> ; <message>`.
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

/// `SGR-Pixels` is preferred, but it is not universally answered, so the
/// terminal's name decides: the terminals that implement it are the ones that
/// prefix their version with these names.
fn mouse_units(probe: &ProbeInfo) -> MouseUnits {
    match probe.sgr_pixels {
        Some(true) => MouseUnits::Pixels,
        Some(false) => MouseUnits::Cells,
        None => {
            let name = probe
                .name
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ["kitty", "ghostty", "wezterm"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                MouseUnits::Pixels
            } else {
                MouseUnits::Cells
            }
        }
    }
}

/// Per-second frame counters, appended to the same log the server writes.
///
/// A pane's terminal is its display, so its own diagnostics cannot go there;
/// the log is where a slow terminal is told apart from a slow compositor.
struct PaneStats {
    log: Option<fs::File>,
    pid: u32,
    window: Instant,
    frames: u32,
    drawn: u32,
    bytes: u64,
    encoded: Duration,
    written: Duration,
    dropped: u32,
}

impl PaneStats {
    fn new(socket: &Path) -> Self {
        Self {
            log: diag::open(&diag::path(socket)).ok(),
            pid: process::id(),
            window: Instant::now(),
            frames: 0,
            drawn: 0,
            bytes: 0,
            encoded: Duration::ZERO,
            written: Duration::ZERO,
            dropped: 0,
        }
    }

    fn line(&mut self, message: &str) {
        let Some(file) = self.log.as_mut() else {
            return;
        };
        let _ = writeln!(file, "{} pane {}: {message}", diag::seconds(), self.pid);
    }

    /// What the terminal said about itself, and the size it is drawn at.
    fn capabilities(&mut self, probe: &ProbeInfo, width: u32, height: u32) {
        self.line(&format!(
            "attached {width}x{height} cell={:?} terminal={:?} graphics={} shared_memory={} \
             sgr_pixels={:?}",
            probe.cell_width.zip(probe.cell_height),
            probe.name,
            probe.graphics,
            probe.shared_memory,
            probe.sgr_pixels,
        ));
    }

    /// One frame taken from the compositor: what it cost to encode and to
    /// write, and whether anything was written at all.
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
        let per_frame = f64::from(frames.max(1));
        self.line(&format!(
            "frames={:.1}/s drawn={drawn} dropped={dropped} unchanged={} out={:.2}MB/s encode={:.1}ms \
             write={:.1}ms",
            f64::from(frames) / elapsed,
            frames.saturating_sub(drawn).saturating_sub(dropped),
            bytes as f64 / elapsed / 1e6,
            encoded.as_secs_f64() * 1000.0 / per_frame,
            written.as_secs_f64() * 1000.0 / per_frame,
        ));
    }
}

/// Attach this process's terminal to a pane socket and run until released.
///
/// # Errors
/// Returns an error when the pane socket cannot be reached, when the terminal
/// cannot be taken over or does not answer the handshake, and when the pane
/// connection fails.  Being released by the server is not an error: the pane
/// prints the reason and returns `Ok`.
#[expect(
    clippy::too_many_lines,
    reason = "the pane's single event loop; every arm shares the same socket, probe and \
              terminal guard"
)]
pub fn attach(socket: &Path, show: Show) -> Result<()> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        Error::io(
            format!(
                "could not reach the pane socket {} (is the server running?)",
                socket.display()
            ),
            error,
        )
    })?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    // A pane killed from outside must still hand the terminal back: raw mode,
    // the alternate screen and mouse reporting all outlive a default-action
    // death, and the shell that comes after it cannot undo them.  The handlers
    // go in before the terminal is taken over, so a signal during the probe is
    // not a window in which the modes outlive the pane.
    let interrupted = diag::termination_flag()?;
    let mut mode = TerminalGuard::enter()?;
    let (probe, shared) = terminal_probe()?;
    if !probe.graphics {
        return Err(Error::GraphicsUnsupported);
    }
    let units = mouse_units(&probe);
    mode.enable_mouse(units)?;
    // The size of a cell is what the probe answered, or the fallback the pane
    // assumes so a terminal that stayed quiet still has a geometry.
    let mut cell = probe.cell_width.zip(probe.cell_height);
    let (width, height) = pane_pixels(
        probe.pixel_width.zip(probe.pixel_height),
        window_pixels(),
        (cols, rows),
        cell.unwrap_or(FALLBACK_CELL),
    );
    let hello = Hello {
        version: protocol::VERSION,
        width,
        height,
        show,
    };

    let mut tx = stream.try_clone()?;
    protocol::send(&mut tx, &PaneToServer::Hello(hello))?;
    let mut presenter = Presenter::new(cell, shared);
    let mut stats = PaneStats::new(socket);
    stats.capabilities(&probe, hello.width, hello.height);

    let handshake_deadline = Instant::now() + Duration::from_secs(1);
    let mut handshake_done = false;
    let stdin = io::stdin();
    let stdout = io::stdout();
    loop {
        if interrupted.load(Ordering::Relaxed) {
            // Something asked the pane to leave; the terminal goes back the
            // same way it does on any other exit.
            mode.restore()?;
            return Ok(());
        }
        if !handshake_done && Instant::now() >= handshake_deadline {
            return Err(Error::PaneHandshakeTimeout);
        }
        // Wait on the terminal, the pane socket and the handshake in one go.
        // Waiting on the socket itself, rather than on a relay thread's
        // channel, is what lets a frame be drawn the moment it arrives.
        let mut fds = [
            PollFd::new(&stdin, PollFlags::IN | PollFlags::HUP | PollFlags::ERR),
            PollFd::new(&stdout, PollFlags::HUP | PollFlags::ERR),
            PollFd::new(&stream, PollFlags::IN | PollFlags::HUP | PollFlags::ERR),
        ];
        // The wait is open-ended once the handshake is answered: every source
        // wakes the poll on its own, so there is nothing to poll for.
        let timeout = (!handshake_done).then(|| {
            let wait = handshake_deadline.saturating_duration_since(Instant::now());
            Timespec {
                tv_sec: wait.as_secs() as i64,
                tv_nsec: wait.subsec_nanos().into(),
            }
        });
        // A signal that is not one of the pane's own (a resize, say) lands
        // here; the flag is read at the top of the loop, so waiting again is
        // all that is left to do.
        match poll(&mut fds, timeout.as_ref()) {
            Err(Errno::INTR) => continue,
            Err(error) => return Err(io::Error::from(error).into()),
            Ok(_) => {}
        }
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR)
            || fds[1].revents().intersects(PollFlags::HUP | PollFlags::ERR)
        {
            return Ok(());
        }
        if fds[2]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
        {
            let Ok(message) = protocol::recv::<ServerToPane>(&mut stream) else {
                mode.restore_with_message("server disconnected")?;
                return Ok(());
            };
            match message {
                ServerToPane::HelloOk => handshake_done = true,
                ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                    mode.restore_with_message(&reason)?;
                    return Ok(());
                }
                ServerToPane::Title(title) => set_title(&title),
                ServerToPane::Cursor(shape) => set_cursor(shape.as_deref()),
                ServerToPane::Frame {
                    width,
                    height,
                    y,
                    rgb,
                } => {
                    let started = Instant::now();
                    let update = presenter.present(width, height, y, rgb);
                    let encoded = started.elapsed();
                    let written = Instant::now();
                    if !update.is_empty() {
                        io::stdout().write_all(&update)?;
                        io::stdout().flush()?;
                    }
                    stats.frame(
                        update.len(),
                        presenter.dropped(),
                        encoded,
                        written.elapsed(),
                    );
                    protocol::send(
                        &mut tx,
                        &PaneToServer::Ack {
                            drawn: !presenter.dropped(),
                        },
                    )?;
                }
            }
        }
        // Everything the terminal has already sent, not one event per wakeup.
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(code) = binding_code(key) {
                        send_key_bits(&mut tx, code, modifier_bits(key.modifiers))?;
                        continue;
                    }
                    if let Some((code, modifiers)) = key_input(key) {
                        send_key_bits(&mut tx, code, modifiers)?;
                    }
                }
                Event::Resize(cols, rows) => {
                    let pixels = window_pixels();
                    // A font zoom changes the cell the terminal draws with
                    // without changing the answer the probe got, so the cell
                    // is derived again from this reading.
                    cell = derived_cell(pixels, (cols, rows)).or(cell);
                    presenter.set_cell_size(cell);
                    let (width, height) =
                        pane_pixels(None, pixels, (cols, rows), cell.unwrap_or(FALLBACK_CELL));
                    protocol::send(&mut tx, &PaneToServer::Resize { width, height })?;
                }
                Event::Mouse(mouse) => {
                    let (pressed, button, scroll) = match mouse.kind {
                        MouseEventKind::Down(b) => (true, Some(b as u8), 0),
                        MouseEventKind::Up(b) => (false, Some(b as u8), 0),
                        MouseEventKind::ScrollUp => (true, None, 15),
                        MouseEventKind::ScrollDown => (true, None, -15),
                        _ => (false, None, 0),
                    };
                    let (x, y) = match units {
                        MouseUnits::Pixels => (f64::from(mouse.column), f64::from(mouse.row)),
                        // Without `SGR-Pixels` the cell is the unit; aim at its
                        // middle so a click lands inside the cell.
                        MouseUnits::Cells => {
                            let (cell_width, cell_height) = cell.unwrap_or(FALLBACK_CELL);
                            let (cell_width, cell_height) =
                                (f64::from(cell_width), f64::from(cell_height));
                            (
                                f64::from(mouse.column).mul_add(cell_width, cell_width / 2.0),
                                f64::from(mouse.row).mul_add(cell_height, cell_height / 2.0),
                            )
                        }
                    };
                    protocol::send(
                        &mut tx,
                        &PaneToServer::Input(Input::Pointer {
                            x,
                            y,
                            button,
                            pressed,
                            scroll,
                        }),
                    )?;
                }
                Event::Paste(text) => {
                    // A paste copied from a system that ends lines with CRLF
                    // is one line break, not two.
                    let mut characters = text.chars().peekable();
                    while let Some(character) = characters.next() {
                        let code = match character {
                            '\r' if characters.peek() == Some(&'\n') => {
                                characters.next();
                                Some(28)
                            }
                            '\n' | '\r' => Some(28),
                            '\t' => Some(15),
                            _ => evdev_char_code(character),
                        };
                        if let Some(code) = code {
                            send_key_bits(&mut tx, code, u8::from(needs_shift(character)))?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The Linux input code and modifier bits a key event carries, or `None` for
/// a key the pane cannot express as one.
fn key_input(key: KeyEvent) -> Option<(u16, u8)> {
    let mut modifiers = key.modifiers;
    if let KeyCode::Char(character) = key.code
        && needs_shift(character)
    {
        // Legacy terminal input reports the shifted character without a
        // modifier, so the shift is restored here.
        modifiers.insert(KeyModifiers::SHIFT);
    }
    let code = match key.code {
        KeyCode::Char(c) => evdev_char_code(c)?,
        KeyCode::Enter => 28,
        KeyCode::Esc => 1,
        KeyCode::Backspace => 14,
        KeyCode::Tab | KeyCode::BackTab => 15,
        KeyCode::Up => 103,
        KeyCode::Down => 108,
        KeyCode::Left => 105,
        KeyCode::Right => 106,
        KeyCode::Home => 102,
        KeyCode::End => 107,
        KeyCode::PageUp => 104,
        KeyCode::PageDown => 109,
        KeyCode::Delete => 111,
        KeyCode::Insert => 110,
        // The function keys are not in one run in evdev: `KEY_F1`..`KEY_F10`
        // are 59..68, `KEY_F11` and `KEY_F12` are 87 and 88, and `KEY_F13` and
        // up continue from 183.
        KeyCode::F(n) => match n {
            1..=10 => 58 + u16::from(n),
            11 => 87,
            12 => 88,
            13..=24 => 170 + u16::from(n),
            _ => return None,
        },
        _ => return None,
    };
    Some((code, modifier_bits(modifiers)))
}

fn send_key_bits(stream: &mut UnixStream, code: u16, modifiers: u8) -> Result<()> {
    protocol::send(
        stream,
        &PaneToServer::Input(Input::Key {
            code,
            pressed: true,
            modifiers,
        }),
    )?;
    protocol::send(
        stream,
        &PaneToServer::Input(Input::Key {
            code,
            pressed: false,
            modifiers,
        }),
    )?;
    Ok(())
}

const fn evdev_char_code(c: char) -> Option<u16> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        '1' | '!' => 2,
        '2' | '@' => 3,
        '3' | '#' => 4,
        '4' | '$' => 5,
        '5' | '%' => 6,
        '6' | '^' => 7,
        '7' | '&' => 8,
        '8' | '*' => 9,
        '9' | '(' => 10,
        '0' | ')' => 11,
        ' ' => 57,
        '-' | '_' => 12,
        '=' | '+' => 13,
        '[' | '{' => 26,
        ']' | '}' => 27,
        ';' | ':' => 39,
        '\'' | '"' => 40,
        '`' | '~' => 41,
        '\\' | '|' => 43,
        ',' | '<' => 51,
        '.' | '>' => 52,
        '/' | '?' => 53,
        _ => return None,
    })
}

/// Whether typing this character needs Shift held on the `us` layout, which is
/// the layout [`evdev_char_code`] is a table of.
fn needs_shift(c: char) -> bool {
    c.is_ascii_uppercase() || "!@#$%^&*()_+{}:\"~|<>?".contains(c)
}

/// The terminal's size in pixels as the kernel last saw it.  Terminals that
/// do not report pixels at all answer with zeros, which is no answer.
fn window_pixels() -> Option<(u32, u32)> {
    terminal::window_size()
        .ok()
        .map(|size| (u32::from(size.width), u32::from(size.height)))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// The pane's size in pixels for a window of `cells` cells.  A pixel size the
/// terminal reported wins over the cells: `reported` is what the terminal
/// answered itself, `fresh` what the kernel was told just now, and a window
/// neither of them describes is its cells times the size of a cell.
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

/// The size of a cell derived from a window reading.  A font zoom changes the
/// cell the terminal draws with and leaves no other trace, so this is how a
/// pane notices one.
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

/// The terminal's modifier flags as the bits the pane protocol packs.
fn modifier_bits(modifiers: KeyModifiers) -> u8 {
    use protocol::modifiers::{ALT, CONTROL, SHIFT, SUPER};
    let mut bits = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, SHIFT),
        (KeyModifiers::CONTROL, CONTROL),
        (KeyModifiers::ALT, ALT),
        (KeyModifiers::SUPER, SUPER),
    ] {
        if modifiers.contains(flag) {
            bits |= bit;
        }
    }
    bits
}

/// `Alt+Q` asks the shown window to close and `Alt+W` detaches, in Linux input
/// codes.  Only without Ctrl or Super, so everything else reaches the client.
fn binding_code(key: KeyEvent) -> Option<u16> {
    if !protocol::modifiers::alt_only(modifier_bits(key.modifiers)) {
        return None;
    }
    match key.code {
        KeyCode::Char('q') => Some(protocol::KEY_Q),
        KeyCode::Char('w') => Some(protocol::KEY_W),
        _ => None,
    }
}

fn set_title(title: &str) {
    let clean = protocol::sanitize_with_limit(title, 512);
    let _ = write!(io::stdout(), "\x1b]2;{clean}\x07");
    let _ = io::stdout().flush();
}

/// The kitty pointer shape: a CSS cursor name, or an empty name to let the
/// terminal draw its own pointer again.
fn set_cursor(shape: Option<&str>) {
    let name: String = shape
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect();
    let _ = write!(io::stdout(), "\x1b]22;{name}\x1b\\");
    let _ = io::stdout().flush();
}

/// The terminal modes a pane turns on, and the exact set it turns back off.
struct TerminalGuard {
    active: bool,
    mouse: Option<MouseUnits>,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // The guard exists before the modes are turned on, so any failure
        // below still undoes what has already been applied.
        let guard = Self {
            active: true,
            mouse: None,
        };
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        io::stdout().write_all(b"\x1b[?7l\x1b[?25l\x1b[?2004h")?;
        io::stdout().flush()?;
        Ok(guard)
    }

    /// Reporting is chosen after the probe, so the mouse is not enabled
    /// together with the other modes.
    fn enable_mouse(&mut self, units: MouseUnits) -> io::Result<()> {
        let modes: &[u8] = match units {
            MouseUnits::Pixels => b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1016h",
            MouseUnits::Cells => b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h",
        };
        io::stdout().write_all(modes)?;
        io::stdout().flush()?;
        self.mouse = Some(units);
        Ok(())
    }

    fn restore_with_message(&mut self, reason: &str) -> io::Result<()> {
        let clean = protocol::sanitize_with_limit(reason, 512);
        self.restore()?;
        writeln!(io::stdout(), "meowland: {clean}")
    }

    /// Undo every mode the pane turned on.  Each step is attempted even after
    /// one of them fails: a shell cannot live with any of them left behind,
    /// and the ones that follow raw mode are exactly the ones a broken write
    /// would otherwise skip.
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
        // Raw mode is the one mode that survives a writer that cannot reach
        // the terminal, so it is undone before any write that could fail.
        note(terminal::disable_raw_mode());
        if let Some(units) = self.mouse.take() {
            let off: &[u8] = match units {
                MouseUnits::Pixels => b"\x1b[?1016l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                MouseUnits::Cells => b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
            };
            note(io::stdout().write_all(off));
        }
        note(io::stdout().write_all(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A,q=2;\x1b\\"));
        note(execute!(
            io::stdout(),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_zero_size_replies() {
        assert_eq!(parse_probe_bytes(b"\x1b[4;0;0t"), ProbeInfo::default());
    }

    #[test]
    fn parses_a_kitty_handshake() {
        let bytes = b"\x1b[6;20;10t\x1b[4;600;800t\
            \x1bP>|kitty(0.48.2)\x1b\\\
            \x1b_Gi=31;OK\x1b\\\
            \x1b_Gi=32;OK\x1b\\\
            \x1b[?1016;2$y\x1b[?62;4;6;22c";
        assert_eq!(
            parse_probe_bytes(bytes),
            ProbeInfo {
                pixel_width: Some(800),
                pixel_height: Some(600),
                cell_width: Some(10),
                cell_height: Some(20),
                graphics: true,
                shared_memory: true,
                sgr_pixels: Some(true),
                name: Some("kitty(0.48.2)".into()),
            }
        );
    }

    #[test]
    fn a_shared_memory_refusal_is_not_a_graphics_refusal() {
        let bytes = b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;ENOENT: no such file\x1b\\";
        let probe = parse_probe_bytes(bytes);
        assert!(probe.graphics);
        assert!(!probe.shared_memory);
    }

    #[test]
    fn records_refused_graphics_and_unsupported_pixel_mouse() {
        let bytes = b"\x1b_Gi=31;ENOTSUP\x1b\\\x1b[?1016;0$y";
        let probe = parse_probe_bytes(bytes);
        assert!(!probe.graphics);
        assert_eq!(probe.sgr_pixels, Some(false));
    }

    #[test]
    fn pixel_mouse_falls_back_to_the_terminal_name() {
        let named = |name: &str| ProbeInfo {
            name: Some(name.into()),
            ..ProbeInfo::default()
        };
        assert_eq!(mouse_units(&named("WezTerm 20240203")), MouseUnits::Pixels);
        assert_eq!(mouse_units(&named("xterm(390)")), MouseUnits::Cells);
        assert_eq!(
            mouse_units(&ProbeInfo {
                sgr_pixels: Some(false),
                name: Some("kitty".into()),
                ..ProbeInfo::default()
            }),
            MouseUnits::Cells
        );
    }

    #[test]
    fn uses_linux_evdev_us_positions() {
        assert_eq!(evdev_char_code('a'), Some(30));
        assert_eq!(evdev_char_code('b'), Some(48));
        assert_eq!(evdev_char_code('q'), Some(16));
        assert_eq!(evdev_char_code('w'), Some(17));
        assert_eq!(evdev_char_code('!'), Some(2));
        assert_eq!(evdev_char_code('?'), Some(53));
    }

    #[test]
    fn shifted_characters_carry_the_shift_bit() {
        let key =
            |character, modifiers| key_input(KeyEvent::new(KeyCode::Char(character), modifiers));
        assert_eq!(key('A', KeyModifiers::NONE), Some((30, 1)));
        assert_eq!(key('!', KeyModifiers::NONE), Some((2, 1)));
        assert_eq!(key('a', KeyModifiers::NONE), Some((30, 0)));
    }

    /// Windows-style F1..F24 are not one run of codes in evdev, and F11 and
    /// F12 in particular sit far away from F10.
    #[test]
    fn function_keys_use_their_evdev_codes() {
        let key = |n| key_input(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE));
        assert_eq!(key(1), Some((59, 0)));
        assert_eq!(key(10), Some((68, 0)));
        assert_eq!(key(11), Some((87, 0)));
        assert_eq!(key(12), Some((88, 0)));
        assert_eq!(key(13), Some((183, 0)));
        assert_eq!(key(24), Some((194, 0)));
        assert_eq!(key(25), None);
    }

    #[test]
    fn shift_tab_is_a_tab_with_the_shift_bit() {
        assert_eq!(
            key_input(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some((15, protocol::modifiers::SHIFT))
        );
    }

    /// The probe's quiet window is armed by the device-attributes reply, and
    /// both the DECRQM reply and the terminal name contain the letters that
    /// a substring match would trip over.
    #[test]
    fn only_a_complete_device_attributes_reply_ends_the_probe() {
        assert!(!device_attributes_seen(b"\x1b[?1016;2$y"));
        assert!(!device_attributes_seen(b"\x1bP>|contour\x1b\\"));
        assert!(!device_attributes_seen(b"\x1b[?62;4;6;22"));
        assert!(device_attributes_seen(b"\x1b[?1016;2$y\x1b[?62;4;6;22c"));
    }

    /// A pane reports the pixels the terminal does, and a cell size derived
    /// from them when the probe's answer is all it has: the two must agree
    /// about the same window.
    #[test]
    fn pane_size_prefers_reported_pixels() {
        assert_eq!(
            pane_pixels(Some((800, 600)), Some((1024, 768)), (80, 24), (10, 20)),
            (800, 600)
        );
        assert_eq!(
            pane_pixels(None, Some((1024, 768)), (80, 24), (10, 20)),
            (1024, 768)
        );
        assert_eq!(pane_pixels(None, None, (80, 24), (10, 20)), (800, 480));
        assert_eq!(derived_cell(Some((1024, 768)), (80, 24)), Some((12, 32)));
        assert_eq!(derived_cell(None, (80, 24)), None);
        assert_eq!(derived_cell(Some((1024, 768)), (0, 24)), None);
    }

    #[test]
    fn paste_shift_matches_us_keymap() {
        assert!(needs_shift('A'));
        assert!(needs_shift('!'));
        assert!(!needs_shift('a'));
        assert!(!needs_shift('1'));
    }
}
