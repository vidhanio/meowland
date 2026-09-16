//! The terminal meowland draws into: capability probing, mode setup and raw
//! escape output.
//!
//! Every escape that shows a pixel or sets a mode goes out through here, as a
//! kitty graphics escape that [`crate::kitty`] encodes.

use std::{
    io::{self, IsTerminal as _, Read as _, Write as _},
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::{
    Error,
    kitty::{GRAPHICS_PROBE_ID, SHARED_PROBE_ID},
};

/// What the terminal reported it can do. Each field is one independent answer.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each of these is an independent thing a terminal can do"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The size of one character cell, in pixels.
    pub cell: (u32, u32),
    /// The terminal's character grid: `(columns, rows)` in cells.
    pub cells: (u32, u32),
    /// `(width, height)` of the whole drawing area in pixels.
    pub pixels: (u32, u32),
    pub terminal: Option<String>,
    pub graphics: bool,
    pub keyboard: bool,
    /// Whether mouse reporting uses pixels (`SGR-Pixels`) instead of cells.
    pub pixel_mouse: bool,
    /// Whether the terminal reads tiles out of shared memory, which keeps their
    /// pixels off the pty.
    pub shared_memory: bool,
}

/// The cell size assumed when the terminal reports none. A wrong guess
/// distorts pixels, not layout, because images are scaled into a cell
/// rectangle.
const FALLBACK_CELL: (u32, u32) = (10, 20);

/// Whether the terminal has gone away.
///
/// A closed terminal hangs its file descriptors up. The input thread cannot
/// report that, because `crossterm`'s event source spins on the error instead
/// of returning from the read. Polling the descriptors takes nothing out of the
/// input, so this is safe while the reader runs.
pub fn hung_up() -> bool {
    let stdin = io::stdin();
    let mut descriptors = [PollFd::new(&stdin, PollFlags::IN)];
    if let Err(error) = poll(&mut descriptors, Some(&Timespec::default())) {
        tracing::debug!(%error, "could not poll the terminal");
        return true;
    }
    let flags = descriptors[0].revents();
    flags.contains(PollFlags::HUP) || flags.contains(PollFlags::ERR)
}

/// How long the probe waits for the terminal's answers.
///
/// A terminal on this machine answers in a few milliseconds. A terminal across
/// a network, such as kitty at the far end of an SSH connection, answers after
/// one round trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The keyboard flags the compositor asks for: disambiguate escape codes,
/// report event types, report all keys as escape codes, report associated text.
const KEYBOARD_FLAGS: u32 = 1 | 2 | 8 | 16;

#[derive(Debug)]
pub struct Terminal {
    capabilities: Capabilities,
    entered: bool,
}

/// The escape that names the window a terminal is in.
///
/// A client supplies the title, so control characters are removed: they would
/// let the client write escapes into the terminal that shows it. The title is
/// also cut to `MAXIMUM_TITLE`.
pub fn title(title: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(title.len() + 8);
    out.extend_from_slice(b"\x1b]2;");
    out.extend(
        title
            .chars()
            .filter(|c| !c.is_control())
            .take(MAXIMUM_TITLE)
            .collect::<String>()
            .bytes(),
    );
    out.push(b'\x07');
    out
}

/// How much of a client's title a terminal is told.
const MAXIMUM_TITLE: usize = 256;

pub fn is_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

impl Terminal {
    /// Read the geometry without changing terminal state.
    pub fn new() -> Result<Self, Error> {
        if !is_terminal() {
            return Err(Error::NotATerminal);
        }
        Ok(Self {
            capabilities: resolve_capabilities(&Probe::default(), window_size()),
            entered: false,
        })
    }

    /// Take over the terminal once there is a window to display.
    pub fn activate(&mut self) -> Result<&Capabilities, Error> {
        if self.entered {
            return Ok(&self.capabilities);
        }
        crossterm::terminal::enable_raw_mode()?;
        let probe = probe().unwrap_or_else(|error| {
            tracing::warn!(%error, "could not probe terminal capabilities");
            Probe::default()
        });
        let capabilities = resolve_capabilities(&probe, window_size());
        if !capabilities.graphics {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(Error::NoGraphics);
        }

        self.capabilities = capabilities;
        if let Err(error) = self.enter() {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(&self.capabilities)
    }

    /// Write escapes to the terminal in one atomic, tear-free write. Frames do
    /// not come through here: this is for the escapes that open and close the
    /// compositor, and for wiping the screen.
    fn write(out: &[u8]) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(out).and_then(|()| stdout.flush())
    }

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

    /// The escapes that wipe the screen, every image and every cell.
    ///
    /// The compositor sends these after a resize, where stale pixels and stale
    /// cell contents cannot be told apart from live ones. The bytes are
    /// returned because the presenter writes them, after the frames already
    /// promised.
    pub fn clear() -> Vec<u8> {
        let mut out = Vec::new();
        crate::kitty::delete_all(&mut out);
        // `CSI 2J` also drops every image the terminal holds, and homes the
        // cursor.
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        out
    }

    fn enter(&mut self) -> io::Result<()> {
        let mut out = Vec::with_capacity(64);
        // The alternate screen keeps the user's scrollback and clears the
        // images on exit.
        out.extend_from_slice(b"\x1b[?1049h");
        // `CSI 22;2t` saves the terminal's title. The title shown here is the
        // client's, and a client with nothing to say says nothing.
        out.extend_from_slice(b"\x1b[22;2t");
        // No autowrap, so a write at the last column cannot scroll the screen
        // and drag the placements along.
        out.extend_from_slice(b"\x1b[?7l");
        out.extend_from_slice(b"\x1b[?25l");
        out.extend_from_slice(b"\x1b[?1003h\x1b[?1006h");
        // Bracketed paste makes pasted text arrive as one paste, not as held
        // keys.
        out.extend_from_slice(b"\x1b[?2004h");
        if self.capabilities.pixel_mouse {
            out.extend_from_slice(b"\x1b[?1016h");
        }
        if self.capabilities.keyboard {
            let _ = write!(out, "\x1b[>{KEYBOARD_FLAGS}u");
        }
        // The screen is cleared before the first placement: `CSI 2J` deletes
        // images too.
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        Self::write(&out)?;
        self.entered = true;
        Ok(())
    }

    fn leave(&self) {
        let mut out = Vec::with_capacity(64);
        crate::kitty::delete_all(&mut out);
        crate::kitty::set_pointer_shape(&mut out, None);
        if self.capabilities.keyboard {
            out.extend_from_slice(b"\x1b[<u");
        }
        out.extend_from_slice(b"\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?2004l");
        out.extend_from_slice(b"\x1b[?7h\x1b[?25h\x1b[?1049l");
        // `CSI 23;2t` restores the title the terminal had before.
        out.extend_from_slice(b"\x1b[23;2t");
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

/// The raw probe answers, before defaults and checks.
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

/// Ask the terminal what it supports, before any other reader touches stdin.
fn probe() -> io::Result<Probe> {
    let mut stdout = io::stdout().lock();
    // Cell size, text area and terminal identity, then the graphics query under
    // its own id, then keyboard protocol and pixel mouse support.
    let mut queries = Vec::new();
    queries.extend_from_slice(b"\x1b[16t\x1b[14t\x1b[>q");
    let _ = write!(
        queries,
        "\x1b_Gi={GRAPHICS_PROBE_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
    );
    queries.extend_from_slice(b"\x1b[?u\x1b[?1016$p");
    stdout.write_all(&queries)?;
    // A terminal does not announce shared memory support, so the probe sends a
    // tile that way and sees whether the terminal read it. The guard removes
    // the object again if it did not.
    let mut shared_probe_bytes = Vec::new();
    let _shared_probe = crate::kitty::shared_memory_probe(&mut shared_probe_bytes);
    stdout.write_all(&shared_probe_bytes)?;
    // Primary device attributes come last, so their answer cannot overtake an
    // earlier one and end the handshake early.
    stdout.write_all(b"\x1b[c")?;
    stdout.flush()?;

    let mut input = io::stdin();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut responses = Vec::new();
    let parsed = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break parse_responses(&responses);
        }
        let stdin = io::stdin();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        let timeout = rustix::event::Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(0),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break parse_responses(&responses),
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        }
        let mut chunk = [0u8; 512];
        match input.read(&mut chunk) {
            Ok(0) => break parse_responses(&responses),
            Ok(n) => responses.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
        let parsed = parse_responses(&responses);
        if parsed.device_attributes {
            break parsed;
        }
    };
    Ok(parsed.probe)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedResponses {
    probe: Probe,
    device_attributes: bool,
}

/// Parse terminal responses. Split out from [`probe`] so its answers can be
/// tested without a terminal.
fn parse_responses(bytes: &[u8]) -> ParsedResponses {
    let mut parsed = ParsedResponses::default();
    let mut rest = bytes;
    while let Some(start) = rest.iter().position(|b| *b == 0x1b) {
        rest = &rest[start..];
        match rest.get(1) {
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
                // DECRQM's `$`) before its final byte.
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
                            // CSI 4 ; height ; width t: the text area in pixels.
                            (Some(4), Some(height), Some(width)) if height > 0 && width > 0 => {
                                parsed.probe.pixels = Some((width, height));
                            }
                            // CSI 6 ; height ; width t: one cell in pixels.
                            (Some(6), Some(height), Some(width)) if height > 0 && width > 0 => {
                                parsed.probe.cell = Some((width, height));
                            }
                            _ => {}
                        }
                    }
                    // CSI ? mode $ y: DECRQM, which reports if a mode is understood.
                    (b'y', params) => {
                        let mut fields = params.split(';');
                        let mode = fields.next().and_then(|p| p.strip_prefix('?'));
                        let answer = fields.next().and_then(|p| p.parse::<u8>().ok());
                        // 0 means "not recognised"; 1 and 2 mean the terminal
                        // has the mode, set or reset.
                        if mode == Some("1016") && answer.is_some_and(|answer| answer > 0) {
                            parsed.probe.pixel_mouse = Some(true);
                        }
                    }
                    // CSI ? flags u: the keyboard enhancement flags the
                    // terminal supports.
                    (b'u', params) => {
                        if let Some(flags) = params.strip_prefix('?') {
                            parsed.probe.keyboard = flags.parse::<u32>().is_ok();
                        }
                    }
                    // CSI ? ... c: the primary device attributes.
                    (b'c', params) => {
                        parsed.device_attributes = params.starts_with('?');
                    }
                    _ => {}
                }
            }
            // DCS > | name(version) ST: XTVERSION.
            Some(b'P') => {
                let Some(end) = find_st(rest) else { break };
                let payload = std::str::from_utf8(&rest[2..end]).unwrap_or_default();
                if let Some(version) = payload.strip_prefix(">|") {
                    parsed.probe.terminal = Some(version.trim().to_owned());
                }
                rest = &rest[end + 2..];
            }
            // APC _ G <control>;<answer> ST: a graphics protocol reply. The
            // answer belongs to the query with that id, so a refused query says
            // nothing about the others.
            Some(b'_') => {
                let Some(end) = find_st(rest) else { break };
                let payload = std::str::from_utf8(&rest[2..end]).unwrap_or_default();
                rest = &rest[end + 2..];
                let Some(reply) = payload.strip_prefix('G') else {
                    continue;
                };
                let (control, answer) = reply.split_once(';').unwrap_or((reply, ""));
                let id = control
                    .split(',')
                    .find_map(|field| field.strip_prefix("i="))
                    .and_then(|id| id.parse::<u32>().ok());
                match (id, answer) {
                    (Some(GRAPHICS_PROBE_ID), "OK") => parsed.probe.graphics = true,
                    (Some(SHARED_PROBE_ID), "OK") => parsed.probe.shared_memory = true,
                    // An error here is a refused query, which is how a terminal
                    // answers the shared memory probe when the object is not on
                    // its machine. That is the answer over SSH.
                    _ => {}
                }
            }
            _ => rest = &rest[1..],
        }
    }
    parsed
}

/// The end of a `ESC \` terminated string, and its start index.
fn find_st(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\x1b\\")
}

/// The cell grid, from the kernel's view of the pty.
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

/// Fill in defaults, and work out what the terminal can be asked for.
///
/// `window` is `(columns, rows, width, height)` from the pty, used when the
/// pixel-size queries go unanswered.
fn resolve_capabilities(probe: &Probe, window: Option<(u32, u32, u32, u32)>) -> Capabilities {
    let (columns, rows, window_width, window_height) = window.unwrap_or((80, 24, 0, 0));
    let cells = (columns.max(1), rows.max(1));
    let pixels = probe
        .pixels
        .or_else(|| {
            (window_width > 0 && window_height > 0).then_some((window_width, window_height))
        })
        .unwrap_or_else(|| {
            let cell = probe.cell.unwrap_or(FALLBACK_CELL);
            (cells.0 * cell.0, cells.1 * cell.1)
        });
    let cell = probe
        .cell
        .unwrap_or_else(|| ((pixels.0 / cells.0).max(1), (pixels.1 / cells.1).max(1)));
    let terminal = probe.terminal.clone();
    // The terminal is asked rather than guessed at by name: Ghostty announces
    // itself as "libghostty", and any terminal can rename itself.
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

    /// Byte-for-byte what kitty 0.48.2 replied to the exact queries [`probe`]
    /// sends, when the shared memory probe's object is on another machine. This
    /// is the answer a kitty at the far end of an SSH connection gives: the
    /// object is in the remote `/dev/shm`, so kitty cannot open it.
    const KITTY_RESPONSES_REMOTE: &[u8] = b"\x1b[6;20;10t\x1b[4;1340;1240t\x1bP>|kitty(0.48.2)\x1b\\\
                                           \x1b_Gi=77;OK\x1b\\\
                                           \x1b_Gi=78;EBADF:Failed to open file for graphics transmission with error: [2] No such file or directory\x1b\\\
                                           \x1b[?0u\x1b[?1016;2$y\x1b[?62;52;c";

    #[test]
    fn a_refused_shared_memory_probe_does_not_deny_graphics() {
        // The graphics query was answered, so the terminal draws. Only the fast
        // path is unavailable, and a terminal answers "OK" to the shared memory
        // probe only when it read the object.
        let parsed = parse_responses(KITTY_RESPONSES_REMOTE);
        assert!(parsed.probe.graphics, "the graphics query was answered");
        assert!(!parsed.probe.shared_memory, "the tile was not read");

        let capabilities = resolve_capabilities(&parsed.probe, Some((124, 67, 1240, 1340)));
        assert!(capabilities.graphics);
        assert!(!capabilities.shared_memory);
    }

    #[test]
    fn a_title_escape_carries_no_escapes_of_its_own() {
        let escape = title("\x1b]2;gotcha\x07\u{9b}31mred");
        assert_eq!(escape, b"\x1b]2;]2;gotcha31mred\x07");
        assert_eq!(title(""), b"\x1b]2;\x07");
        assert!(title(&"x".repeat(MAXIMUM_TITLE * 2)).len() < MAXIMUM_TITLE + 8);
    }

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
        // attributes alone.
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
        // A terminal that answered nothing: 80x25 cells of 1000x800 pixels is
        // a 12x32 cell.
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

        let capabilities = resolve_capabilities(&Probe::default(), None);
        assert_eq!(capabilities.cells, (80, 24));
        assert_eq!(
            capabilities.pixels,
            (80 * FALLBACK_CELL.0, 24 * FALLBACK_CELL.1)
        );
    }

    #[test]
    fn a_zero_sized_answer_is_ignored() {
        // Some terminals answer 0 for the size, which taken literally leaves an
        // empty screen.
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
