//! Terminal mode ownership, pane geometry, and terminal control output.

use std::io::{self, Write};

use crossterm::{
    event::{self, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute, terminal,
};
use meowland_protocol as protocol;

/// Cell size assumed when the terminal reports none.
pub const FALLBACK_CELL: (u16, u16) = (10, 20);

const FALLBACK_CELLS: (u16, u16) = (80, 24);

pub const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
const ASSOCIATED_TEXT: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::from_bits_retain(1 << 4);
pub const REQUESTED_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KEYBOARD_FLAGS.union(ASSOCIATED_TEXT);

pub fn cells() -> (u16, u16) {
    terminal::size().unwrap_or(FALLBACK_CELLS)
}

/// Kernel-reported window pixels; zero dimensions mean unavailable.
pub fn window_pixels() -> Option<(u32, u32)> {
    terminal::window_size()
        .ok()
        .map(|size| (u32::from(size.width), u32::from(size.height)))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// Prefer terminal-reported pixels, then fresh kernel pixels, then cell
/// geometry.
pub fn pane_pixels(
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
pub fn derived_cell(pixels: Option<(u32, u32)>, cells: (u16, u16)) -> Option<(u16, u16)> {
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

pub fn title_bytes(title: &str) -> Vec<u8> {
    let clean = protocol::sanitize_with_limit(title, 512);
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// Set the kitty pointer shape; an empty name restores the terminal default.
pub fn cursor_bytes(shape: Option<&str>) -> Vec<u8> {
    let name: String = shape
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect();
    format!("\x1b]22;{name}\x1b\\").into_bytes()
}

/// Restores the terminal modes enabled by this pane.
pub struct TerminalGuard {
    active: bool,
    mouse: bool,
    clipboard: Option<bool>,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
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
            PushKeyboardEnhancementFlags(REQUESTED_KEYBOARD_FLAGS),
            event::EnableFocusChange
        )?;
        io::stdout().write_all(b"\x1b[?7l\x1b[?25l\x1b[?2004h")?;
        io::stdout().flush()?;
        Ok(guard)
    }

    pub fn enable_mouse(&mut self) -> io::Result<()> {
        self.mouse = true;
        io::stdout().write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1016h")?;
        io::stdout().flush()?;
        Ok(())
    }

    pub fn enable_clipboard(&mut self, previous: bool) -> io::Result<()> {
        self.clipboard = Some(previous);
        io::stdout().write_all(b"\x1b[?5522h")?;
        io::stdout().flush()
    }

    /// Cancel interrupted control strings and end synchronized updates before
    /// restoring modes. Attempt every step even if an earlier one fails.
    pub fn restore(&mut self) -> io::Result<()> {
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
        note(io::stdout().write_all(b"\x18\x1b\\"));
        note(execute!(io::stdout(), terminal::EndSynchronizedUpdate));
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
