//! Terminal mode ownership, pane geometry, and terminal control output.

use std::io::{self, Write};

use crossterm::{
    event::{self, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    queue, terminal,
};
use meowland_core::pixels::FrameSize;
use meowland_protocol as protocol;

use crate::{Error, Result};

/// Longest sanitized title or status line forwarded to the terminal.
///
/// Titles and close reasons come from the server, so this bounds how much one
/// untrusted control sequence can add to the output queue.
pub(crate) const MAX_TEXT_CHARS: usize = 512;
/// Longest pointer-shape name forwarded to the terminal.
const MAX_CURSOR_NAME_CHARS: usize = 64;

/// Cell size assumed when the terminal reports none.
pub(crate) const FALLBACK_CELL: (u16, u16) = (10, 20);

const FALLBACK_CELLS: (u16, u16) = (80, 24);

pub(crate) const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
const ASSOCIATED_TEXT: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::from_bits_retain(1 << 4);
pub(crate) const REQUESTED_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KEYBOARD_FLAGS.union(ASSOCIATED_TEXT);

pub(crate) fn cells() -> (u16, u16) {
    terminal::size().unwrap_or(FALLBACK_CELLS)
}

/// Kernel-reported window pixels; zero dimensions mean unavailable.
pub(crate) fn window_pixels() -> Option<(u32, u32)> {
    terminal::window_size()
        .ok()
        .map(|size| (u32::from(size.width), u32::from(size.height)))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// Prefer terminal-reported pixels, then fresh kernel pixels, then cell
/// geometry.
pub(crate) fn pane_pixels(
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

/// Validate pane dimensions before sending them to the compositor.
pub(crate) fn frame_size(width: u32, height: u32) -> Result<FrameSize> {
    FrameSize::new(width, height).ok_or(Error::InvalidPaneSize { width, height })
}

/// Recalculate cell size from window pixels after font zoom.
pub(crate) fn derived_cell(pixels: Option<(u32, u32)>, cells: (u16, u16)) -> Option<(u16, u16)> {
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

pub(crate) fn title_bytes(title: &str) -> Vec<u8> {
    let clean = protocol::sanitize_with_limit(title, MAX_TEXT_CHARS);
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// Set the kitty pointer shape; an empty name restores the terminal default.
pub(crate) fn cursor_bytes(shape: Option<&str>) -> Vec<u8> {
    let name: String = shape
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_CURSOR_NAME_CHARS)
        .collect();
    format!("\x1b]22;{name}\x1b\\").into_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardMode {
    Enabled,
    Disabled,
}

/// Restores the terminal modes enabled by this pane.
pub(crate) struct TerminalGuard {
    active: bool,
    mouse: bool,
    clipboard: Option<ClipboardMode>,
}

impl TerminalGuard {
    /// Enter raw mode and append the sequences that switch pane modes on.
    ///
    /// Escape sequences are appended to `out`, the session's ordered output
    /// queue, so they can never interleave with a partially written frame. Raw
    /// mode itself is a termios change and takes effect immediately.
    pub(crate) fn enter(out: &mut Vec<u8>) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self {
            active: true,
            mouse: false,
            clipboard: None,
        };
        queue!(
            out,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            PushKeyboardEnhancementFlags(REQUESTED_KEYBOARD_FLAGS),
            event::EnableFocusChange
        )
        .expect("writing terminal commands to a byte buffer cannot fail");
        out.extend_from_slice(b"\x1b[?7l\x1b[?25l\x1b[?2004h");
        Ok(guard)
    }

    pub(crate) fn enable_mouse(&mut self, out: &mut Vec<u8>) {
        self.mouse = true;
        out.extend_from_slice(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1016h");
    }

    pub(crate) fn enable_clipboard(&mut self, previous: ClipboardMode, out: &mut Vec<u8>) {
        self.clipboard = Some(previous);
        out.extend_from_slice(b"\x1b[?5522h");
    }

    /// Append the sequences that undo this guard and disable raw mode.
    ///
    /// Cancel interrupted control strings and end synchronized updates before
    /// restoring modes. Every step is appended even though disabling raw mode
    /// can fail; the first failure is returned once all bytes are queued.
    pub(crate) fn restore_into(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let raw = terminal::disable_raw_mode();
        out.extend_from_slice(b"\x18\x1b\\");
        queue!(out, terminal::EndSynchronizedUpdate)
            .expect("writing terminal commands to a byte buffer cannot fail");
        if self.mouse {
            self.mouse = false;
            out.extend_from_slice(b"\x1b[?1016l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
        }
        if let Some(previous) = self.clipboard.take() {
            out.extend_from_slice(if previous == ClipboardMode::Enabled {
                b"\x1b[?5522h"
            } else {
                b"\x1b[?5522l"
            });
        }
        out.extend_from_slice(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A,q=2;\x1b\\");
        queue!(
            out,
            event::DisableFocusChange,
            event::PopKeyboardEnhancementFlags,
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen
        )
        .expect("writing terminal commands to a byte buffer cannot fail");
        raw
    }

    /// Best-effort synchronous restore for unwinding or error paths.
    fn restore_blocking(&mut self) {
        let mut bytes = Vec::new();
        let _ = self.restore_into(&mut bytes);
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore_blocking();
    }
}
