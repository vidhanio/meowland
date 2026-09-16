//! Map terminal keys to Linux key codes for the advertised `us` keymap.

use crossterm::event::{KeyCode as TerminalKeyCode, ModifierKeyCode};
use evdev::KeyCode;

/// A key code, with the shift state that the intended symbol needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    /// Linux input event code (`KEY_*`).
    pub code: KeyCode,
    /// Whether shift must be held while it is pressed.
    pub shift: bool,
}

/// Offset between Linux input event codes and the key codes that XKB, and so
/// the Wayland keyboard protocol, uses.
///
/// Terminals and the `KEY_*` constants use evdev. A keymap compiled from the
/// `evdev` rules addresses keys as `evdev + 8`, and that is the number the seat
/// must be given. A wrong offset does not fail loudly: it types the character
/// eight keys away from the one that was pressed.
pub const XKB_OFFSET: u32 = 8;

const fn stroke(code: u32, shift: bool) -> KeyStroke {
    KeyStroke {
        code: KeyCode::new(code as u16),
        shift,
    }
}

/// Mouse button codes, as the Wayland pointer protocol names them.
pub mod button {
    use super::KeyCode;
    /// `BTN_LEFT`.
    pub const LEFT: KeyCode = KeyCode::BTN_LEFT;
    /// `BTN_RIGHT`.
    pub const RIGHT: KeyCode = KeyCode::BTN_RIGHT;
    /// `BTN_MIDDLE`.
    pub const MIDDLE: KeyCode = KeyCode::BTN_MIDDLE;
}

/// Key codes for the modifier keys, so that a client sees real presses and not
/// only modifier state.
pub mod modifier {
    use super::KeyCode;
    /// `KEY_LEFTSHIFT`.
    pub const LEFT_SHIFT: KeyCode = KeyCode::KEY_LEFTSHIFT;
    /// `KEY_LEFTSHIFT`'s sibling.
    pub const RIGHT_SHIFT: KeyCode = KeyCode::KEY_RIGHTSHIFT;
    /// `KEY_LEFTCTRL`.
    pub const LEFT_CTRL: KeyCode = KeyCode::KEY_LEFTCTRL;
    /// `KEY_RIGHTCTRL`.
    pub const RIGHT_CTRL: KeyCode = KeyCode::KEY_RIGHTCTRL;
    /// `KEY_LEFTALT`.
    pub const LEFT_ALT: KeyCode = KeyCode::KEY_LEFTALT;
    /// `KEY_RIGHTALT`.
    pub const RIGHT_ALT: KeyCode = KeyCode::KEY_RIGHTALT;
    /// `KEY_LEFTMETA`.
    pub const LEFT_META: KeyCode = KeyCode::KEY_LEFTMETA;
    /// `KEY_RIGHTMETA`.
    pub const RIGHT_META: KeyCode = KeyCode::KEY_RIGHTMETA;
}

/// Map a character from the terminal to the `us` stroke that types it.
pub fn for_char(c: char) -> Option<KeyStroke> {
    if let 'a'..='z' | 'A'..='Z' = c {
        let lowercase = c.to_ascii_lowercase();
        let index = usize::from(lowercase as u8 - b'a');
        return Some(stroke(LETTERS[index], c.is_ascii_uppercase()));
    }

    Some(match c {
        '1'..='9' => stroke(2 + (c as u32 - '1' as u32), false),
        '0' => stroke(11, false),
        // The shifted digit row, in the order the layout puts it.
        '!' => stroke(2, true),
        '@' => stroke(3, true),
        '#' => stroke(4, true),
        '$' => stroke(5, true),
        '%' => stroke(6, true),
        '^' => stroke(7, true),
        '&' => stroke(8, true),
        '*' => stroke(9, true),
        '(' => stroke(10, true),
        ')' => stroke(11, true),
        '-' => stroke(12, false),
        '_' => stroke(12, true),
        '=' => stroke(13, false),
        '+' => stroke(13, true),
        '\t' => stroke(15, false),
        '[' => stroke(26, false),
        '{' => stroke(26, true),
        ']' => stroke(27, false),
        '}' => stroke(27, true),
        '\r' => stroke(28, false),
        ';' => stroke(39, false),
        ':' => stroke(39, true),
        '\'' => stroke(40, false),
        '"' => stroke(40, true),
        '`' => stroke(41, false),
        '~' => stroke(41, true),
        '\\' => stroke(43, false),
        '|' => stroke(43, true),
        ',' => stroke(51, false),
        '<' => stroke(51, true),
        '.' => stroke(52, false),
        '>' => stroke(52, true),
        '/' => stroke(53, false),
        '?' => stroke(53, true),
        ' ' => stroke(57, false),
        _ => return None,
    })
}

/// The key codes of `F1` to `F12`. The last two sit apart from the rest, so a
/// table is shorter than arithmetic.
const FUNCTION: [u32; 12] = [59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88];

/// The key code of each letter, in alphabet order, on a `us` keyboard. There is
/// no formula, because the letters lie in three rows.
const LETTERS: [u32; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// Map the non-character keys the terminal reports.
pub fn for_key(code: TerminalKeyCode) -> Option<KeyStroke> {
    Some(match code {
        TerminalKeyCode::Esc => stroke(1, false),
        TerminalKeyCode::Enter => stroke(28, false),
        TerminalKeyCode::Tab => stroke(15, false),
        TerminalKeyCode::BackTab => stroke(15, true),
        TerminalKeyCode::Backspace => stroke(14, false),
        TerminalKeyCode::Insert => stroke(110, false),
        TerminalKeyCode::Delete => stroke(111, false),
        TerminalKeyCode::Home => stroke(102, false),
        TerminalKeyCode::End => stroke(107, false),
        TerminalKeyCode::PageUp => stroke(104, false),
        TerminalKeyCode::PageDown => stroke(109, false),
        TerminalKeyCode::Up => stroke(103, false),
        TerminalKeyCode::Down => stroke(108, false),
        TerminalKeyCode::Left => stroke(105, false),
        TerminalKeyCode::Right => stroke(106, false),
        TerminalKeyCode::CapsLock => stroke(58, false),
        TerminalKeyCode::ScrollLock => stroke(70, false),
        TerminalKeyCode::NumLock => stroke(69, false),
        TerminalKeyCode::PrintScreen => stroke(99, false),
        TerminalKeyCode::Pause => stroke(119, false),
        TerminalKeyCode::Menu => stroke(127, false),
        TerminalKeyCode::F(n) => stroke(*FUNCTION.get(usize::from(n).checked_sub(1)?)?, false),
        TerminalKeyCode::Modifier(modifier) => {
            stroke(u32::from(for_modifier(modifier)?.code()), false)
        }
        TerminalKeyCode::Char(c) => return for_char(c),
        _ => return None,
    })
}

/// The key code of a modifier that the terminal reports as its own event.
pub const fn for_modifier(modifier: ModifierKeyCode) -> Option<KeyCode> {
    Some(match modifier {
        ModifierKeyCode::LeftShift => modifier::LEFT_SHIFT,
        ModifierKeyCode::RightShift => modifier::RIGHT_SHIFT,
        ModifierKeyCode::LeftControl => modifier::LEFT_CTRL,
        ModifierKeyCode::RightControl => modifier::RIGHT_CTRL,
        ModifierKeyCode::LeftAlt => modifier::LEFT_ALT,
        ModifierKeyCode::RightAlt => modifier::RIGHT_ALT,
        ModifierKeyCode::LeftSuper => modifier::LEFT_META,
        ModifierKeyCode::RightSuper => modifier::RIGHT_META,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode as TerminalKeyCode, ModifierKeyCode};

    use super::*;

    #[test]
    fn shifted_symbols_sit_next_to_their_unshifted_neighbours() {
        // A pair that shares a key on the `us` layout must use one key code,
        // and only the shifted symbol may need shift. That keeps typing
        // independent of the terminal's layout.
        for (plain, shifted) in [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
            ('0', ')'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (']', '}'),
            ('\\', '|'),
            (';', ':'),
            ('\'', '"'),
            ('`', '~'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
        ] {
            let plain = for_char(plain).expect("plain symbol must be typeable");
            let shifted = for_char(shifted).expect("shifted symbol must be typeable");
            assert_eq!(
                plain.code, shifted.code,
                "{shifted:?} should share a key with its unshifted neighbour"
            );
            assert!(!plain.shift, "the plain symbol must not need shift");
            assert!(shifted.shift, "the shifted symbol must need shift");
        }
    }

    #[test]
    fn letters_and_digits_are_where_the_us_layout_puts_them() {
        assert_eq!(for_char('a'), Some(stroke(30, false)));
        assert_eq!(for_char('l'), Some(stroke(38, false)));
        assert_eq!(for_char('z'), Some(stroke(44, false)));
        assert_eq!(for_char('m'), Some(stroke(50, false)));
        assert_eq!(for_char('q'), Some(stroke(16, false)));
        assert_eq!(for_char('A'), Some(stroke(30, true)));
        assert_eq!(for_char('Z'), Some(stroke(44, true)));
        assert_eq!(for_char('1'), Some(stroke(2, false)));
        assert_eq!(for_char('0'), Some(stroke(11, false)));
        assert_eq!(for_char(' '), Some(stroke(57, false)));
    }

    #[test]
    fn every_printable_ascii_character_is_typeable() {
        for c in ' '..='~' {
            assert!(for_char(c).is_some(), "{c:?} must be typeable");
        }
        for c in ['\t', '\r'] {
            assert!(for_char(c).is_some(), "{c:?} must be typeable");
        }
    }

    #[test]
    fn untypeable_characters_are_reported_as_such() {
        // No key in the advertised `us` keymap produces these.
        for c in ['é', '日', '😼', '\u{7f}'] {
            assert_eq!(for_char(c), None);
        }
    }

    #[test]
    fn named_keys_map_to_their_input_codes() {
        assert_eq!(for_key(TerminalKeyCode::Esc), Some(stroke(1, false)));
        assert_eq!(for_key(TerminalKeyCode::Enter), Some(stroke(28, false)));
        assert_eq!(for_key(TerminalKeyCode::Backspace), Some(stroke(14, false)));
        assert_eq!(for_key(TerminalKeyCode::Left), Some(stroke(105, false)));
        assert_eq!(for_key(TerminalKeyCode::Down), Some(stroke(108, false)));
        assert_eq!(for_key(TerminalKeyCode::F(1)), Some(stroke(59, false)));
        assert_eq!(for_key(TerminalKeyCode::F(11)), Some(stroke(87, false)));
        assert_eq!(for_key(TerminalKeyCode::F(12)), Some(stroke(88, false)));
        assert_eq!(
            for_key(TerminalKeyCode::Modifier(ModifierKeyCode::LeftShift)),
            Some(KeyStroke {
                code: modifier::LEFT_SHIFT,
                shift: false,
            })
        );
    }
}
