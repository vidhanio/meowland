//! The terminal we draw into: capability probing, mode setup and raw escape
//! output.
//!
//! Everything meowland shows goes through here, either as terminal text (the
//! status line) or as kitty graphics escapes (window pixels, see
//! [`crate::kitty`]).

use std::{
    io::{self, IsTerminal as _, Read as _, Write as _},
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, poll};

/// What the terminal told us it can do.
///
/// A bag of answers rather than a state machine: each is something the terminal
/// either does or does not, and they vary independently of one another.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each of these is an independent thing a terminal can do"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Size of one character cell in pixels.
    pub cell: (u32, u32),
    /// The terminal's character grid: `(columns, rows)` in cells.
    pub cells: (u32, u32),
    /// `(width, height)` of the whole drawing area in pixels.
    pub pixels: (u32, u32),
    /// Terminal name and version, from the `XTVERSION` report.
    pub terminal: Option<String>,
    /// Whether the terminal answered the graphics query.
    pub graphics: bool,
    /// Whether the terminal speaks the kitty keyboard protocol.
    pub keyboard: bool,
    /// Whether mouse reporting can be done in pixels (`SGR-Pixels`) rather than
    /// cells.
    pub pixel_mouse: bool,
    /// Whether the terminal reads tiles out of a shared memory object, which is
    /// what keeps their pixels off the pty.
    pub shared_memory: bool,
}

/// Cell size assumed when the terminal does not report one. Only affects
/// crispness: images are always scaled into a cell rectangle, so a wrong guess
/// distorts pixels but not layout.
const FALLBACK_CELL: (u32, u32) = (10, 20);

/// How long to wait for the terminal to answer the capability queries.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Keyboard flags we ask for: disambiguate escape codes, report event types,
/// report all keys as escape codes, report associated text.
const KEYBOARD_FLAGS: u32 = 1 | 2 | 8 | 16;

pub struct Terminal {
    capabilities: Capabilities,
    entered: bool,
}

/// Why meowland cannot draw in this terminal.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// There is nothing to draw into.
    #[error("meowland needs a terminal on stdin and stdout (try running it directly)")]
    NotATerminal,
    /// The terminal would not let go of line-based input.
    #[error("could not put the terminal into raw mode")]
    RawMode(#[source] io::Error),
    /// The terminal does not implement the protocol the pixels go out through.
    #[error(
        "this terminal does not support the kitty graphics protocol (meowland needs kitty, \
         ghostty, or another terminal that implements it)"
    )]
    NoGraphics,
}

impl Terminal {
    /// Take over the terminal: raw mode, alternate screen, mouse and keyboard
    /// reporting.
    pub fn new() -> Result<Self, Error> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(Error::NotATerminal);
        }
        crossterm::terminal::enable_raw_mode().map_err(Error::RawMode)?;
        let probe = probe().unwrap_or_default();
        let capabilities = resolve_capabilities(&probe, window_size());
        if !capabilities.graphics {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(Error::NoGraphics);
        }

        let mut terminal = Self {
            capabilities,
            entered: false,
        };
        terminal.enter();
        Ok(terminal)
    }

    /// Capabilities discovered at startup.
    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Write escapes to the terminal in one atomic, tear-free write.
    ///
    /// Frames do not go through here - the presenter writes those - so this is
    /// for the escapes that bracket the compositor's life and for wiping the
    /// screen, both of which happen when nothing else is being written.
    fn write(out: &[u8]) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(out).and_then(|()| stdout.flush())
    }

    /// Re-read the terminal size after a resize. Returns the new capabilities.
    pub fn refresh(&mut self) -> &Capabilities {
        if let Ok(size) = crossterm::terminal::window_size() {
            if size.columns > 0 {
                self.capabilities.cells = (u32::from(size.columns), u32::from(size.rows));
            }
            if size.width > 0 {
                self.capabilities.pixels = (u32::from(size.width), u32::from(size.height));
            }
        }
        &self.capabilities
    }

    /// The escapes that wipe the screen: every image and every cell.
    ///
    /// Used when the terminal changed size, where stale pixels and stale cell
    /// contents cannot be told apart from live ones. Built rather than written
    /// because the presenter sends it, so that it lands after the frames the
    /// terminal has already been promised.
    pub fn clear() -> Vec<u8> {
        let mut out = Vec::new();
        crate::kitty::delete_all(&mut out);
        // `CSI 2J` also drops any images the terminal still holds, and homes
        // the cursor.
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        out
    }

    /// Take over the terminal: alternate screen, no autowrap, mouse and pointer
    /// shapes.
    fn enter(&mut self) {
        let mut out = Vec::with_capacity(64);
        // Alternate screen keeps the user's scrollback intact and clears images
        // on the way out.
        out.extend_from_slice(b"\x1b[?1049h");
        // No autowrap: a stray write at the last column must never scroll the
        // screen, because scrolling would drag our placements along
        // with the text.
        out.extend_from_slice(b"\x1b[?7l");
        out.extend_from_slice(b"\x1b[?25l");
        out.extend_from_slice(b"\x1b[?1003h\x1b[?1006h");
        // Bracketed paste: pasted text arrives as a paste, not as a burst of
        // held keys.
        out.extend_from_slice(b"\x1b[?2004h");
        if self.capabilities.pixel_mouse {
            out.extend_from_slice(b"\x1b[?1016h");
        }
        if self.capabilities.keyboard {
            let _ = write!(out, "\x1b[>{KEYBOARD_FLAGS}u");
        }
        // Clear the screen *before* the first placement: `CSI 2J` also deletes
        // images.
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        let _ = Self::write(&out);
        self.entered = true;
    }

    /// Undo everything [`Terminal::enter`] did.
    fn leave(&self) {
        let mut out = Vec::with_capacity(64);
        crate::kitty::delete_all(&mut out);
        crate::kitty::set_pointer_shape(&mut out, None);
        if self.capabilities.keyboard {
            out.extend_from_slice(b"\x1b[<u");
        }
        out.extend_from_slice(b"\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?2004l");
        out.extend_from_slice(b"\x1b[?7h\x1b[?25h\x1b[?1049l");
        let _ = Self::write(&out);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.entered {
            self.leave();
        }
    }
}

/// Answer to the startup queries, before defaults and sanity checks are
/// applied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Probe {
    cell: Option<(u32, u32)>,
    pixels: Option<(u32, u32)>,
    terminal: Option<String>,
    graphics: bool,
    /// Whether the terminal read the tile sent out of shared memory.
    shared_memory: bool,
    keyboard: bool,
    /// Whether the terminal answered the `SGR-Pixels` mode query.
    pixel_mouse: Option<bool>,
}

/// Ask the terminal what it supports. Runs before any other reader touches
/// stdin.
fn probe() -> io::Result<Probe> {
    let mut stdout = io::stdout().lock();
    // Cell size and text area, terminal identity, graphics support, keyboard
    // protocol, and finally primary device attributes as the "everything is
    // answered now" marker.
    stdout.write_all(
        b"\x1b[16t\x1b[14t\x1b[>q\
          \x1b_Gi=77,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\
          \x1b[?u\
          \x1b[?1016$p\
          \x1b[c",
    )?;
    // Whether tiles can come out of shared memory is not something a terminal
    // announces: it has to be asked, by sending a tile that way and seeing
    // whether it says it read it. The object goes with the answer.
    let mut shared_probe = Vec::new();
    let shared = crate::kitty::shared_memory_probe(&mut shared_probe);
    stdout.write_all(&shared_probe)?;
    stdout.flush()?;

    let mut input = io::stdin();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut responses = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let stdin = io::stdin();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        let timeout = rustix::event::Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(0),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        }
        let mut chunk = [0u8; 512];
        match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => responses.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
        // Primary device attributes are answered last, so its arrival ends the
        // handshake.
        if parse_responses(&responses).device_attributes {
            break;
        }
    }
    let mut probe = parse_responses(&responses).probe;
    // The tile sent above is either read and unlinked by the terminal, or still
    // sitting in shared memory with nobody having looked at it.
    // By id, not by shape: the graphics query answers "OK" too, and it is the
    // tile that has to have been read.
    let expected = format!("\x1b_Gi={};OK\x1b\\", crate::kitty::SHARED_PROBE_ID);
    probe.shared_memory = shared
        && responses
            .windows(expected.len())
            .any(|window| window == expected.as_bytes());
    if !probe.shared_memory {
        crate::kitty::discard_shared_probe();
    }
    Ok(probe)
}

/// The parsed subset of the probe answers we care about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedResponses {
    probe: Probe,
    device_attributes: bool,
}

/// Parse terminal responses. Split out from [`probe`] so the exact byte
/// sequences a terminal sends can be tested without a terminal.
fn parse_responses(bytes: &[u8]) -> ParsedResponses {
    let mut parsed = ParsedResponses::default();
    let mut rest = bytes;
    while let Some(start) = rest.iter().position(|b| *b == 0x1b) {
        rest = &rest[start..];
        match rest.get(1) {
            // CSI: escape [ params final
            Some(b'[') => {
                let Some(final_index) = rest
                    .iter()
                    .skip(2)
                    .position(|b| (0x40..=0x7e).contains(b))
                    .map(|index| index + 2)
                else {
                    break;
                };
                let final_byte = rest[final_index];
                // A CSI can carry intermediate bytes (0x20..=0x2f, like
                // DECRQM's `$`) between its parameters and its
                // final byte; they are not part of the parameters.
                let params = std::str::from_utf8(&rest[2..final_index])
                    .unwrap_or_default()
                    .trim_end_matches(|c: char| ('\u{20}'..='\u{2f}').contains(&c));
                rest = &rest[final_index + 1..];
                match (final_byte, params) {
                    (b't', params) => {
                        let mut parts = params.split(';');
                        let kind = parts.next().and_then(|p| p.parse::<u8>().ok());
                        let first = parts.next().and_then(|p| p.parse().ok());
                        let second = parts.next().and_then(|p| p.parse().ok());
                        match (kind, first, second) {
                            // CSI 4 ; height ; width t — text area in pixels.
                            (Some(4), Some(height), Some(width)) if height > 0 && width > 0 => {
                                parsed.probe.pixels = Some((width, height));
                            }
                            // CSI 6 ; height ; width t — a single cell in pixels.
                            (Some(6), Some(height), Some(width)) if height > 0 && width > 0 => {
                                parsed.probe.cell = Some((width, height));
                            }
                            _ => {}
                        }
                    }
                    // CSI ? mode $ y — DECRQM: is this mode understood, and is it set?
                    (b'y', params) => {
                        let mut fields = params.split(';');
                        let mode = fields.next().and_then(|p| p.strip_prefix('?'));
                        let answer = fields.next().and_then(|p| p.parse::<u8>().ok());
                        // 0 means "not recognised", 1 "set", 2 "reset": either
                        // of the latter
                        // means the terminal has the mode, which is all we need
                        // to know.
                        if mode == Some("1016") && answer.is_some_and(|answer| answer > 0) {
                            parsed.probe.pixel_mouse = Some(true);
                        }
                    }
                    // CSI ? flags u — keyboard enhancement flags the terminal supports.
                    (b'u', params) => {
                        if let Some(flags) = params.strip_prefix('?') {
                            parsed.probe.keyboard = flags.parse::<u32>().is_ok();
                        }
                    }
                    // CSI ? … c — primary device attributes.
                    (b'c', params) => {
                        parsed.device_attributes = params.starts_with('?');
                    }
                    _ => {}
                }
            }
            // DCS > | name(version) ST — XTVERSION.
            Some(b'P') => {
                let Some(end) = find_st(rest) else { break };
                let payload = std::str::from_utf8(&rest[2..end]).unwrap_or_default();
                if let Some(version) = payload.strip_prefix(">|") {
                    parsed.probe.terminal = Some(version.trim().to_owned());
                }
                rest = &rest[end + 2..];
            }
            // APC _ G … ST — graphics protocol replies.
            Some(b'_') => {
                let Some(end) = find_st(rest) else { break };
                let payload = std::str::from_utf8(&rest[2..end]).unwrap_or_default();
                if payload.starts_with('G') {
                    parsed.probe.graphics = payload.ends_with("OK");
                }
                rest = &rest[end + 2..];
            }
            _ => rest = &rest[1..],
        }
    }
    parsed
}

/// Find the end of a `ESC \` terminated string, returning its start index.
fn find_st(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\x1b\\")
}

/// The terminal's cell grid, from the kernel's view of the pty.
fn window_size() -> Option<(u32, u32, u32, u32)> {
    crossterm::terminal::window_size().ok().map(|size| {
        (
            u32::from(size.columns).max(1),
            u32::from(size.rows).max(1),
            u32::from(size.width),
            u32::from(size.height),
        )
    })
}

/// Fill in defaults and work out what we can actually ask the terminal for.
///
/// `window` is `(columns, rows, width, height)` as reported by the pty, which
/// is what is left when the terminal does not answer the pixel-size queries.
fn resolve_capabilities(probe: &Probe, window: Option<(u32, u32, u32, u32)>) -> Capabilities {
    let (columns, rows, window_width, window_height) = window.unwrap_or((80, 24, 0, 0));
    let cells = (columns.max(1), rows.max(1));
    let pixels = probe
        .pixels
        .or_else(|| Some((window_width, window_height)).filter(|(w, h)| *w > 0 && *h > 0))
        .unwrap_or_else(|| {
            let cell = probe.cell.unwrap_or(FALLBACK_CELL);
            (cells.0 * cell.0, cells.1 * cell.1)
        });
    let cell = probe
        .cell
        .unwrap_or_else(|| ((pixels.0 / cells.0).max(1), (pixels.1 / cells.1).max(1)));
    let terminal = probe.terminal.clone();
    // Ask the terminal rather than guess from its name: Ghostty announces
    // itself as "libghostty", and every other terminal is free to rename
    // itself too.
    let pixel_mouse = probe.pixel_mouse.unwrap_or_else(|| {
        terminal.as_deref().is_some_and(|name| {
            name.starts_with("kitty") || name.starts_with("ghostty") || name.starts_with("WezTerm")
        })
    });
    Capabilities {
        cell,
        cells,
        pixels,
        terminal,
        graphics: probe.graphics,
        shared_memory: probe.shared_memory,
        keyboard: probe.keyboard,
        pixel_mouse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte what kitty 0.48.2 replied to the exact queries [`probe`]
    /// sends.
    const KITTY_RESPONSES: &[u8] = b"\x1b[6;20;10t\x1b[4;1340;1240t\x1bP>|kitty(0.48.2)\x1b\\\
                                     \x1b_Gi=77;OK\x1b\\\x1b[?0u\x1b[?1016;2$y\x1b[?62;52;c";

    #[test]
    fn parses_a_kitty_handshake() {
        let parsed = parse_responses(KITTY_RESPONSES);
        assert!(parsed.device_attributes);
        assert_eq!(
            parsed.probe,
            Probe {
                shared_memory: false,
                cell: Some((10, 20)),
                pixels: Some((1240, 1340)),
                terminal: Some("kitty(0.48.2)".to_owned()),
                graphics: true,
                keyboard: true,
                pixel_mouse: Some(true),
            }
        );
    }

    #[test]
    fn parses_a_silent_terminal() {
        // A terminal that understands none of the queries answers the device
        // attributes only.
        let parsed = parse_responses(b"\x1b[?1;2c");
        assert!(parsed.device_attributes);
        assert!(!parsed.probe.graphics);
        assert_eq!(parsed.probe.cell, None);
    }

    #[test]
    fn parses_a_partial_handshake_without_panicking() {
        for length in 0..KITTY_RESPONSES.len() {
            let _ = parse_responses(&KITTY_RESPONSES[..length]);
        }
    }

    #[test]
    fn derives_the_cell_size_when_the_terminal_omits_it() {
        // A terminal that answered nothing: 80x24 cells of 1000x800 pixels is a
        // 12x33 cell.
        let capabilities = resolve_capabilities(
            &Probe {
                graphics: true,
                ..Probe::default()
            },
            Some((80, 25, 1000, 800)),
        );
        assert_eq!(capabilities.cells, (80, 25));
        assert_eq!(capabilities.pixels, (1000, 800));
        assert_eq!(capabilities.cell, (12, 32));

        // Nothing at all known: the fallback cell size, with a sane grid.
        let capabilities = resolve_capabilities(&Probe::default(), None);
        assert_eq!(capabilities.cells, (80, 24));
        assert_eq!(
            capabilities.pixels,
            (80 * FALLBACK_CELL.0, 24 * FALLBACK_CELL.1)
        );
    }

    #[test]
    fn a_zero_sized_answer_is_ignored() {
        // The protocol notes that some terminals answer 0 for the size; taking
        // that literally would leave the compositor with an empty
        // screen.
        let parsed = parse_responses(b"\x1b[4;0;0t\x1b[6;0;0t\x1b[?1;2c");
        assert_eq!(parsed.probe.pixels, None);
        assert_eq!(parsed.probe.cell, None);
        let capabilities = resolve_capabilities(&parsed.probe, Some((80, 24, 800, 480)));
        assert_eq!(capabilities.pixels, (800, 480));
    }

    #[test]
    fn a_probed_cell_size_wins_over_the_derived_one() {
        let capabilities = resolve_capabilities(
            &Probe {
                cell: Some((10, 20)),
                pixels: Some((1240, 1340)),
                graphics: true,
                ..Probe::default()
            },
            Some((124, 67, 1240, 1340)),
        );
        assert_eq!(capabilities.cell, (10, 20));
        assert_eq!(capabilities.cells, (124, 67));
    }
}
