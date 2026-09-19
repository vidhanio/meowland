//! Terminal side of a meowland pane.
//!
//! The terminal is deliberately kept in this module: the server never needs
//! to know about raw mode, terminal escape sequences, or crossterm events.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::{execute, terminal};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::kitty::Presenter;
use crate::protocol::{self, Hello, Input, PaneToServer, ServerToPane, Show};

const CELL_WIDTH: u16 = 10;
const CELL_HEIGHT: u16 = 20;

/// Answers extracted from terminal size negotiation replies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProbeInfo {
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default)]
struct TerminalProbe {
    size: ProbeInfo,
    graphics: bool,
}

fn terminal_probe() -> io::Result<TerminalProbe> {
    let mut out = io::stdout();
    out.write_all(b"\x1b[16t\x1b[14t\x1b_Ga=q,f=24,s=1,v=1,i=31;AAAA\x1b\\\x1b[c")?;
    out.flush()?;
    let stdin = io::stdin();
    let mut fds = [PollFd::new(
        &stdin,
        PollFlags::IN | PollFlags::HUP | PollFlags::ERR,
    )];
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    while Instant::now() < deadline && bytes.len() < 4096 {
        let left = deadline.saturating_duration_since(Instant::now());
        let timeout = Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: left.subsec_nanos().into(),
        };
        if poll(&mut fds, Some(&timeout))? == 0 {
            break;
        }
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR) {
            break;
        }
        let mut byte = [0u8; 1];
        match (&stdin).read(&mut byte) {
            Ok(1) => {
                bytes.push(byte[0]);
                if kitty_ok(&bytes) && da_seen(&bytes) {
                    break;
                }
            }
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(TerminalProbe {
        graphics: kitty_ok(&bytes),
        size: parse_probe_bytes(&bytes),
    })
}

fn kitty_ok(bytes: &[u8]) -> bool {
    let Some(start) = bytes.windows(2).position(|w| w == b"\x1b_G") else {
        return false;
    };
    let rest = &bytes[start..];
    let end = rest[2..]
        .iter()
        .position(|&b| b == 0x1b)
        .map_or(rest.len(), |n| n + 2);
    rest[..end].windows(2).any(|w| w == b"OK") && rest[..end].windows(4).any(|w| w == b"i=31")
}

fn da_seen(bytes: &[u8]) -> bool {
    let Some(start) = bytes
        .windows(3)
        .position(|w| w[0] == 0x1b && w[1] == b'[' && w[2] == b'?')
    else {
        return false;
    };
    bytes[start + 3..].contains(&b'c')
}

/// Parse the two xterm size replies. Keeping this parser independent makes it
/// useful with captured terminal handshakes and avoids depending on timing.
pub fn parse_probe_bytes(bytes: &[u8]) -> ProbeInfo {
    let mut out = ProbeInfo::default();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b || i + 3 >= bytes.len() || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let Some(end_rel) = bytes[start..].iter().position(|&b| b == b't') else {
            break;
        };
        let end = start + end_rel;
        let body = &bytes[start..end];
        let Some(semi) = body.iter().position(|&b| b == b';') else {
            i = end + 1;
            continue;
        };
        let Some(kind) = std::str::from_utf8(&body[..semi])
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            i = end + 1;
            continue;
        };
        let nums: Vec<u32> = body[semi + 1..]
            .split(|&b| b == b';')
            .filter_map(|p| std::str::from_utf8(p).ok()?.parse().ok())
            .collect();
        if nums.len() >= 2 && nums[0] != 0 && nums[1] != 0 {
            match kind {
                4 => {
                    out.pixel_height = Some(nums[0]);
                    out.pixel_width = Some(nums[1]);
                }
                6 => {
                    out.cell_height = u16::try_from(nums[0]).ok();
                    out.cell_width = u16::try_from(nums[1]).ok();
                }
                _ => {}
            }
        }
        i = end + 1;
    }
    out
}

/// Attach this process's terminal to a pane socket and run until released.
#[allow(clippy::too_many_lines)]
pub fn attach(socket: &Path, show: Show) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket)?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut mode = TerminalGuard::enter()?;
    let probe = terminal_probe()?;
    if !probe.graphics {
        return Err(anyhow::anyhow!("terminal does not support kitty graphics"));
    }
    let hello = Hello {
        version: protocol::VERSION,
        width: probe.size.pixel_width.unwrap_or_else(|| {
            u32::from(cols) * u32::from(probe.size.cell_width.unwrap_or(CELL_WIDTH))
        }),
        height: probe.size.pixel_height.unwrap_or_else(|| {
            u32::from(rows) * u32::from(probe.size.cell_height.unwrap_or(CELL_HEIGHT))
        }),
        cell_width: probe.size.cell_width,
        cell_height: probe.size.cell_height,
        show,
    };

    let mut tx = stream.try_clone()?;
    protocol::send(&mut tx, &PaneToServer::Hello(hello))?;
    let (server_tx, server_rx) = mpsc::channel();
    let mut presenter = Presenter::new(probe.size.cell_width.zip(probe.size.cell_height));
    let cell_width = u32::from(probe.size.cell_width.unwrap_or(CELL_WIDTH));
    let cell_height = u32::from(probe.size.cell_height.unwrap_or(CELL_HEIGHT));
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
        let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
        if poll(&mut fds, Some(&zero))? > 0
            && fds.iter().any(|fd| fd.revents().intersects(PollFlags::HUP | PollFlags::ERR))
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
                    if is_binding(key, KeyCode::Char('q')) || is_binding(key, KeyCode::Char('w')) {
                        let code = if matches!(key.code, KeyCode::Char('q')) {
                            16
                        } else {
                            17
                        };
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
                    let cw = probe.size.cell_width;
                    let ch = probe.size.cell_height;
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
                    protocol::send(
                        &mut tx,
                        &PaneToServer::Input(Input::Pointer {
                            x: f64::from(mouse.column) * f64::from(cell_width),
                            y: f64::from(mouse.row) * f64::from(cell_height),
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
        modifiers: modifier_bits(key.modifiers),
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

fn needs_shift(c: char) -> bool {
    c.is_ascii_uppercase() || "!@#$%^&*()_+{}:\"~|<>?".contains(c)
}

fn modifier_bits(m: KeyModifiers) -> u8 {
    u8::from(m.contains(KeyModifiers::SHIFT))
        | (u8::from(m.contains(KeyModifiers::CONTROL)) << 1)
        | (u8::from(m.contains(KeyModifiers::ALT)) << 2)
        | (u8::from(m.contains(KeyModifiers::SUPER)) << 3)
}
fn is_binding(k: KeyEvent, c: KeyCode) -> bool {
    k.code == c
        && k.modifiers.contains(KeyModifiers::ALT)
        && !k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

fn set_title(title: &str) {
    let clean: String = title
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
    print!("\x1b]2;{clean}\x07");
    let _ = io::stdout().flush();
}

struct TerminalGuard {
    active: bool,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::event::EnableMouseCapture
        )?;
        print!("\x1b[?7l\x1b[?25l\x1b[?2004h");
        io::stdout().flush()?;
        Ok(Self { active: true })
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
            io::stdout().write_all(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A;\x1b\\")?;
            execute!(
                io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::cursor::Show,
                terminal::LeaveAlternateScreen
            )?;
            terminal::disable_raw_mode()?;
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
        assert_eq!(
            parse_probe_bytes(b"\x1b[4;600;800t\x1b[6;20;10t"),
            ProbeInfo {
                pixel_width: Some(800),
                pixel_height: Some(600),
                cell_width: Some(10),
                cell_height: Some(20)
            }
        );
        assert_eq!(parse_probe_bytes(b"\x1b[4;0;0t"), ProbeInfo::default());
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
    fn paste_shift_matches_us_keymap() {
        assert!(needs_shift('A'));
        assert!(needs_shift('!'));
        assert!(!needs_shift('a'));
        assert!(!needs_shift('1'));
    }
}
