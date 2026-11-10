//! Terminal mode ownership, pane geometry, and terminal control output.

use std::{cell::Cell, io, rc::Rc};

use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::{
    event::{self, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    queue, terminal,
};
use meowland_core::pixels::FrameSize;
use meowland_protocol as protocol;

use crate::{Error, Result, output};

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

/// Copy text to the terminal's clipboard with OSC 52.
pub(crate) fn copy_bytes(text: &[u8]) -> Vec<u8> {
    let mut sequence = String::from("\x1b]52;c;");
    STANDARD.encode_string(text, &mut sequence);
    sequence.push('\x07');
    sequence.into_bytes()
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

/// Pushes the window title onto the terminal's title stack.
const TITLE_PUSH: &[u8] = b"\x1b[22;0t";
/// Pops the title stack back to the title saved by [`TITLE_PUSH`].
pub(crate) const TITLE_POP: &[u8] = b"\x1b[23;0t";

/// Where the title push ends in the output stream, so a stalled restore pops
/// the title stack only if the push reached the terminal and the queued pop
/// did not.
#[derive(Clone)]
pub(crate) struct TitlePush {
    written: Rc<Cell<usize>>,
    end: usize,
}

impl TitlePush {
    fn landed(&self) -> bool {
        self.written.get() >= self.end
    }

    /// Whether a pop queued to end at stream position `pop_end` must be
    /// written again.
    pub(crate) fn needs_pop(&self, pop_end: usize) -> bool {
        self.landed() && self.written.get() < pop_end
    }
}

/// Modes this pane switched on, undone by [`TerminalGuard::restore`].
struct Modes {
    mouse: bool,
    title: TitlePush,
}

/// Sequences undoing a [`TerminalGuard`], without the title pop.
pub(crate) struct Restore {
    /// Mode restore sequence to queue or write directly.
    pub(crate) bytes: Vec<u8>,
    /// Result of leaving raw mode, which happens immediately.
    pub(crate) raw: io::Result<()>,
    pub(crate) title: TitlePush,
}

/// Restores the terminal modes enabled by this pane, best effort on drop.
pub(crate) struct TerminalGuard {
    modes: Option<Modes>,
}

impl TerminalGuard {
    /// Enter raw mode and append the sequences that switch pane modes on.
    ///
    /// Input typed while the terminal showed the waiting status is discarded,
    /// so it never reaches the window about to be shown.
    ///
    /// Escape sequences are appended to `out`, the session's ordered output
    /// queue ending at stream position `end`, so they can never interleave
    /// with a partially written frame. Raw mode itself is a termios change and
    /// takes effect immediately.
    pub(crate) fn enter(
        out: &mut Vec<u8>,
        written: Rc<Cell<usize>>,
        end: usize,
    ) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        rustix::termios::tcflush(io::stdin(), rustix::termios::QueueSelector::IFlush)?;
        Self::append_enter(out);
        Ok(Self {
            modes: Some(Modes {
                mouse: false,
                title: TitlePush {
                    written,
                    end: end + TITLE_PUSH.len(),
                },
            }),
        })
    }

    fn append_enter(out: &mut Vec<u8>) {
        out.extend_from_slice(TITLE_PUSH);
        queue!(
            out,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            PushKeyboardEnhancementFlags(REQUESTED_KEYBOARD_FLAGS),
            event::EnableFocusChange
        )
        .expect("writing terminal commands to a byte buffer cannot fail");
        out.extend_from_slice(b"\x1b[?7l\x1b[?25l\x1b[?2004h");
    }

    pub(crate) fn enable_mouse(&mut self, out: &mut Vec<u8>) {
        if let Some(modes) = &mut self.modes {
            modes.mouse = true;
            out.extend_from_slice(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1016h");
        }
    }

    /// Leave raw mode and return the sequences that undo the pane modes.
    ///
    /// The sequences first cancel interrupted control strings and end
    /// synchronized updates. Raw mode is left even if building the sequences
    /// is all that succeeds; its result is reported separately.
    pub(crate) fn restore(mut self) -> Restore {
        let modes = self
            .modes
            .take()
            .expect("only restoring or dropping a guard takes its modes");
        let raw = terminal::disable_raw_mode();
        let mut bytes = Vec::new();
        append_restore(&modes, &mut bytes);
        Restore {
            bytes,
            raw,
            title: modes.title,
        }
    }
}

fn append_restore(modes: &Modes, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x18\x1b\\");
    queue!(out, terminal::EndSynchronizedUpdate)
        .expect("writing terminal commands to a byte buffer cannot fail");
    if modes.mouse {
        out.extend_from_slice(b"\x1b[?1016l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
    }
    out.extend_from_slice(b"\x1b[?2004l\x1b[?7h\x1b_Ga=d,d=A,q=2;\x1b\\");
    out.extend_from_slice(&cursor_bytes(None));
    queue!(
        out,
        event::DisableFocusChange,
        event::PopKeyboardEnhancementFlags,
        crossterm::cursor::Show,
        terminal::LeaveAlternateScreen
    )
    .expect("writing terminal commands to a byte buffer cannot fail");
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let Some(modes) = self.modes.take() else {
            return;
        };
        let _ = terminal::disable_raw_mode();
        let mut bytes = Vec::new();
        append_restore(&modes, &mut bytes);
        if modes.title.landed() {
            bytes.extend_from_slice(TITLE_POP);
        }
        output::write_best_effort(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|part| part == needle)
    }

    #[test]
    fn presentation_pushes_the_title_first() {
        let mut entered = Vec::new();
        TerminalGuard::append_enter(&mut entered);
        assert!(entered.starts_with(TITLE_PUSH));
    }

    #[test]
    fn only_a_landed_push_with_an_unwritten_pop_is_popped_again() {
        let written = Rc::new(Cell::new(7));
        let title = TitlePush {
            written: Rc::clone(&written),
            end: 8,
        };
        assert!(!title.needs_pop(30), "the push never reached the terminal");
        written.set(29);
        assert!(title.needs_pop(30));
        written.set(30);
        assert!(!title.needs_pop(30), "the queued pop already landed");
    }

    #[test]
    fn copy_writes_base64_text_to_the_clipboard_selection() {
        assert_eq!(copy_bytes(b"hi\n"), b"\x1b]52;c;aGkK\x07");
    }

    #[test]
    fn restoration_disables_mouse_reporting_only_when_enabled() {
        let title = TitlePush {
            written: Rc::default(),
            end: 0,
        };
        let mut bytes = Vec::new();
        append_restore(
            &Modes {
                mouse: true,
                title: title.clone(),
            },
            &mut bytes,
        );
        assert!(contains(&bytes, b"\x1b[?1016l"));

        let mut bytes = Vec::new();
        append_restore(
            &Modes {
                mouse: false,
                title,
            },
            &mut bytes,
        );
        assert!(!contains(&bytes, b"\x1b[?1016l"));
    }
}
