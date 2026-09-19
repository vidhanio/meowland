//! Terminal side of a meowland pane.
//!
//! The terminal is deliberately kept in this module: the server never needs
//! to know about raw mode, terminal escape sequences, or crossterm events.

use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::mpsc::{self, TryRecvError},
    thread,
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
    kitty::{Presenter, SharedMemory},
    protocol::{self, Hello, Input, PaneToServer, ServerToPane, Show},
};

const CELL_WIDTH: u16 = 10;
const CELL_HEIGHT: u16 = 20;
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
            Err(error) => return Err(error.into()),
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

fn device_attributes_seen(bytes: &[u8]) -> bool {
    let Some(start) = find(bytes, b"\x1b[?") else {
        return false;
    };
    bytes[start + 3..].contains(&b'c')
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
pub fn attach(socket: &Path, show: Show) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket)?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut mode = TerminalGuard::enter()?;
    let (probe, shared) = terminal_probe()?;
    if !probe.graphics {
        return Err(anyhow::anyhow!("terminal does not support kitty graphics"));
    }
    let units = mouse_units(&probe);
    mode.enable_mouse(units)?;
    let window = terminal::window_size().ok();
    let window_pixels = window.map(|size| (u32::from(size.width), u32::from(size.height)));
    let cell_width = u32::from(probe.cell_width.unwrap_or(CELL_WIDTH));
    let cell_height = u32::from(probe.cell_height.unwrap_or(CELL_HEIGHT));
    let hello = Hello {
        version: protocol::VERSION,
        width: probe
            .pixel_width
            .or_else(|| window_pixels.map(|(width, _)| width).filter(|w| *w > 0))
            .unwrap_or_else(|| u32::from(cols) * cell_width),
        height: probe
            .pixel_height
            .or_else(|| window_pixels.map(|(_, height)| height).filter(|h| *h > 0))
            .unwrap_or_else(|| u32::from(rows) * cell_height),
        cell_width: probe.cell_width,
        cell_height: probe.cell_height,
        show,
    };

    let mut tx = stream.try_clone()?;
    protocol::send(&mut tx, &PaneToServer::Hello(hello))?;
    let (server_tx, server_rx) = mpsc::channel();
    let mut presenter = Presenter::new(probe.cell_width.zip(probe.cell_height), shared);
    thread::Builder::new()
        .name("meowland-pane-rx".into())
        .spawn(move || {
            let mut stream = stream;
            while let Ok(message) = protocol::recv::<ServerToPane>(&mut stream) {
                if server_tx.send(message).is_err() {
                    break;
                }
            }
        })?;

    let handshake_deadline = Instant::now() + Duration::from_secs(1);
    let mut handshake_done = false;
    let stdin = io::stdin();
    let stdout = io::stdout();
    loop {
        let mut fds = [
            PollFd::new(&stdin, PollFlags::HUP | PollFlags::ERR),
            PollFd::new(&stdout, PollFlags::HUP | PollFlags::ERR),
        ];
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if poll(&mut fds, Some(&zero))? > 0
            && fds
                .iter()
                .any(|fd| fd.revents().intersects(PollFlags::HUP | PollFlags::ERR))
        {
            return Ok(());
        }
        loop {
            let message = match server_rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    mode.restore_with_message("server disconnected")?;
                    return Ok(());
                }
            };
            match message {
                ServerToPane::HelloOk => handshake_done = true,
                ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                    mode.restore_with_message(&reason)?;
                    return Ok(());
                }
                ServerToPane::Title(title) => set_title(&title),
                ServerToPane::Cursor(shape) => set_cursor(shape.as_deref()),
                ServerToPane::Frame { width, height, rgb } => {
                    let update = presenter.present(width, height, rgb);
                    if !update.is_empty() {
                        io::stdout().write_all(&update)?;
                        io::stdout().flush()?;
                    }
                    protocol::send(&mut tx, &PaneToServer::Ack)?;
                }
            }
        }
        if !handshake_done && Instant::now() >= handshake_deadline {
            return Err(anyhow::anyhow!("pane handshake timed out"));
        }
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(code) = binding_code(key) {
                        send_key(&mut tx, code, key.modifiers)?;
                        continue;
                    }
                    if let Some(Input::Key {
                        code, modifiers, ..
                    }) = key_input(key)
                    {
                        send_key_bits(&mut tx, code, modifiers)?;
                    }
                }
                Event::Resize(w, h) => {
                    let cw = probe.cell_width;
                    let ch = probe.cell_height;
                    protocol::send(
                        &mut tx,
                        &PaneToServer::Resize {
                            width: u32::from(w) * cell_width,
                            height: u32::from(h) * cell_height,
                            cell_width: cw,
                            cell_height: ch,
                        },
                    )?;
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
                        MouseUnits::Cells => (
                            f64::from(mouse.column)
                                .mul_add(f64::from(cell_width), f64::from(cell_width) / 2.0),
                            f64::from(mouse.row)
                                .mul_add(f64::from(cell_height), f64::from(cell_height) / 2.0),
                        ),
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
                    for character in text.chars() {
                        let code = match character {
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

fn key_input(key: KeyEvent) -> Option<Input> {
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
        KeyCode::Tab => 15,
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
        KeyCode::F(n) => 58 + u16::from(n),
        _ => return None,
    };
    Some(Input::Key {
        code,
        pressed: true,
        modifiers: modifier_bits(modifiers),
    })
}

fn send_key(stream: &mut UnixStream, code: u16, modifiers: KeyModifiers) -> anyhow::Result<()> {
    send_key_bits(stream, code, modifier_bits(modifiers))
}

fn send_key_bits(stream: &mut UnixStream, code: u16, modifiers: u8) -> anyhow::Result<()> {
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
    let alt_alone = key.modifiers.contains(KeyModifiers::ALT)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    if !alt_alone {
        return None;
    }
    match key.code {
        KeyCode::Char('q') => Some(protocol::KEY_Q),
        KeyCode::Char('w') => Some(protocol::KEY_W),
        _ => None,
    }
}

fn set_title(title: &str) {
    let clean: String = title
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
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
        let clean: String = reason
            .chars()
            .filter(|c| !c.is_control())
            .take(512)
            .collect();
        self.restore()?;
        writeln!(io::stdout(), "meowland: {clean}")
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.active {
            // Raw mode is the one mode that survives a writer that cannot
            // reach the terminal, and a shell cannot live with it, so it is
            // undone before any write that could fail.
            terminal::disable_raw_mode()?;
            if let Some(units) = self.mouse.take() {
                let off: &[u8] = match units {
                    MouseUnits::Pixels => b"\x1b[?1016l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                    MouseUnits::Cells => b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                };
                io::stdout().write_all(off)?;
            }
            io::stdout().write_all(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A,q=2;\x1b\\")?;
            execute!(
                io::stdout(),
                crossterm::cursor::Show,
                terminal::LeaveAlternateScreen
            )?;
            self.active = false;
        }
        Ok(())
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
    fn parses_sizes_and_ignores_zero() {
        let expected = ProbeInfo {
            pixel_width: Some(800),
            pixel_height: Some(600),
            cell_width: Some(10),
            cell_height: Some(20),
            ..ProbeInfo::default()
        };
        assert_eq!(parse_probe_bytes(b"\x1b[4;600;800t\x1b[6;20;10t"), expected);
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
        assert!(matches!(
            key('A', KeyModifiers::NONE),
            Some(Input::Key {
                code: 30,
                modifiers: 1,
                ..
            })
        ));
        assert!(matches!(
            key('!', KeyModifiers::NONE),
            Some(Input::Key {
                code: 2,
                modifiers: 1,
                ..
            })
        ));
        assert!(matches!(
            key('a', KeyModifiers::NONE),
            Some(Input::Key {
                code: 30,
                modifiers: 0,
                ..
            })
        ));
    }

    #[test]
    fn paste_shift_matches_us_keymap() {
        assert!(needs_shift('A'));
        assert!(needs_shift('!'));
        assert!(!needs_shift('a'));
        assert!(!needs_shift('1'));
    }
}
