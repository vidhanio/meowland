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

const fn stroke(code: KeyCode, shift: bool) -> KeyStroke {
    KeyStroke { code, shift }
}

/// Map a character from the terminal to the `us` stroke that types it.
pub fn for_char(c: char) -> Option<KeyStroke> {
    if let 'a'..='z' | 'A'..='Z' = c {
        let lowercase = c.to_ascii_lowercase();
        let index = usize::from(lowercase as u8 - b'a');
        return Some(stroke(LETTERS[index], c.is_ascii_uppercase()));
    }

    Some(match c {
        '1'..='9' => stroke(DIGITS[usize::from(c as u8 - b'1')], false),
        '0' => stroke(KeyCode::KEY_0, false),
        // The shifted digit row, in the order the layout puts it.
        '!' => stroke(KeyCode::KEY_1, true),
        '@' => stroke(KeyCode::KEY_2, true),
        '#' => stroke(KeyCode::KEY_3, true),
        '$' => stroke(KeyCode::KEY_4, true),
        '%' => stroke(KeyCode::KEY_5, true),
        '^' => stroke(KeyCode::KEY_6, true),
        '&' => stroke(KeyCode::KEY_7, true),
        '*' => stroke(KeyCode::KEY_8, true),
        '(' => stroke(KeyCode::KEY_9, true),
        ')' => stroke(KeyCode::KEY_0, true),
        '-' => stroke(KeyCode::KEY_MINUS, false),
        '_' => stroke(KeyCode::KEY_MINUS, true),
        '=' => stroke(KeyCode::KEY_EQUAL, false),
        '+' => stroke(KeyCode::KEY_EQUAL, true),
        '\t' => stroke(KeyCode::KEY_TAB, false),
        '[' => stroke(KeyCode::KEY_LEFTBRACE, false),
        '{' => stroke(KeyCode::KEY_LEFTBRACE, true),
        ']' => stroke(KeyCode::KEY_RIGHTBRACE, false),
        '}' => stroke(KeyCode::KEY_RIGHTBRACE, true),
        '\r' => stroke(KeyCode::KEY_ENTER, false),
        ';' => stroke(KeyCode::KEY_SEMICOLON, false),
        ':' => stroke(KeyCode::KEY_SEMICOLON, true),
        '\'' => stroke(KeyCode::KEY_APOSTROPHE, false),
        '"' => stroke(KeyCode::KEY_APOSTROPHE, true),
        '`' => stroke(KeyCode::KEY_GRAVE, false),
        '~' => stroke(KeyCode::KEY_GRAVE, true),
        '\\' => stroke(KeyCode::KEY_BACKSLASH, false),
        '|' => stroke(KeyCode::KEY_BACKSLASH, true),
        ',' => stroke(KeyCode::KEY_COMMA, false),
        '<' => stroke(KeyCode::KEY_COMMA, true),
        '.' => stroke(KeyCode::KEY_DOT, false),
        '>' => stroke(KeyCode::KEY_DOT, true),
        '/' => stroke(KeyCode::KEY_SLASH, false),
        '?' => stroke(KeyCode::KEY_SLASH, true),
        ' ' => stroke(KeyCode::KEY_SPACE, false),
        _ => return None,
    })
}

/// The key codes of `F1` to `F12`. The last two sit apart from the rest, so a
/// table is shorter than arithmetic.
const FUNCTION: [KeyCode; 12] = [
    KeyCode::KEY_F1,
    KeyCode::KEY_F2,
    KeyCode::KEY_F3,
    KeyCode::KEY_F4,
    KeyCode::KEY_F5,
    KeyCode::KEY_F6,
    KeyCode::KEY_F7,
    KeyCode::KEY_F8,
    KeyCode::KEY_F9,
    KeyCode::KEY_F10,
    KeyCode::KEY_F11,
    KeyCode::KEY_F12,
];

/// The key code of each letter, in alphabet order, on a `us` keyboard. There is
/// no formula, because the letters lie in three rows.
const LETTERS: [KeyCode; 26] = [
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
];

/// The key code of each digit key, `1` to `9`.
const DIGITS: [KeyCode; 9] = [
    KeyCode::KEY_1,
    KeyCode::KEY_2,
    KeyCode::KEY_3,
    KeyCode::KEY_4,
    KeyCode::KEY_5,
    KeyCode::KEY_6,
    KeyCode::KEY_7,
    KeyCode::KEY_8,
    KeyCode::KEY_9,
];

/// Map the non-character keys the terminal reports.
pub fn for_key(code: TerminalKeyCode) -> Option<KeyStroke> {
    Some(match code {
        TerminalKeyCode::Esc => stroke(KeyCode::KEY_ESC, false),
        TerminalKeyCode::Enter => stroke(KeyCode::KEY_ENTER, false),
        TerminalKeyCode::Tab => stroke(KeyCode::KEY_TAB, false),
        TerminalKeyCode::BackTab => stroke(KeyCode::KEY_TAB, true),
        TerminalKeyCode::Backspace => stroke(KeyCode::KEY_BACKSPACE, false),
        TerminalKeyCode::Insert => stroke(KeyCode::KEY_INSERT, false),
        TerminalKeyCode::Delete => stroke(KeyCode::KEY_DELETE, false),
        TerminalKeyCode::Home => stroke(KeyCode::KEY_HOME, false),
        TerminalKeyCode::End => stroke(KeyCode::KEY_END, false),
        TerminalKeyCode::PageUp => stroke(KeyCode::KEY_PAGEUP, false),
        TerminalKeyCode::PageDown => stroke(KeyCode::KEY_PAGEDOWN, false),
        TerminalKeyCode::Up => stroke(KeyCode::KEY_UP, false),
        TerminalKeyCode::Down => stroke(KeyCode::KEY_DOWN, false),
        TerminalKeyCode::Left => stroke(KeyCode::KEY_LEFT, false),
        TerminalKeyCode::Right => stroke(KeyCode::KEY_RIGHT, false),
        TerminalKeyCode::CapsLock => stroke(KeyCode::KEY_CAPSLOCK, false),
        TerminalKeyCode::ScrollLock => stroke(KeyCode::KEY_SCROLLLOCK, false),
        TerminalKeyCode::NumLock => stroke(KeyCode::KEY_NUMLOCK, false),
        TerminalKeyCode::PrintScreen => stroke(KeyCode::KEY_SYSRQ, false),
        TerminalKeyCode::Pause => stroke(KeyCode::KEY_PAUSE, false),
        TerminalKeyCode::Menu => stroke(KeyCode::KEY_COMPOSE, false),
        TerminalKeyCode::F(n) => stroke(*FUNCTION.get(usize::from(n).checked_sub(1)?)?, false),
        TerminalKeyCode::Modifier(modifier) => stroke(for_modifier(modifier)?, false),
        TerminalKeyCode::Char(c) => return for_char(c),
        _ => return None,
    })
}

/// The key code of a modifier that the terminal reports as its own event.
pub const fn for_modifier(modifier: ModifierKeyCode) -> Option<KeyCode> {
    Some(match modifier {
        ModifierKeyCode::LeftShift => KeyCode::KEY_LEFTSHIFT,
        ModifierKeyCode::RightShift => KeyCode::KEY_RIGHTSHIFT,
        ModifierKeyCode::LeftControl => KeyCode::KEY_LEFTCTRL,
        ModifierKeyCode::RightControl => KeyCode::KEY_RIGHTCTRL,
        ModifierKeyCode::LeftAlt => KeyCode::KEY_LEFTALT,
        ModifierKeyCode::RightAlt => KeyCode::KEY_RIGHTALT,
        ModifierKeyCode::LeftSuper => KeyCode::KEY_LEFTMETA,
        ModifierKeyCode::RightSuper => KeyCode::KEY_RIGHTMETA,
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
        assert_eq!(for_char('a'), Some(stroke(KeyCode::KEY_A, false)));
        assert_eq!(for_char('l'), Some(stroke(KeyCode::KEY_L, false)));
        assert_eq!(for_char('z'), Some(stroke(KeyCode::KEY_Z, false)));
        assert_eq!(for_char('m'), Some(stroke(KeyCode::KEY_M, false)));
        assert_eq!(for_char('q'), Some(stroke(KeyCode::KEY_Q, false)));
        assert_eq!(for_char('A'), Some(stroke(KeyCode::KEY_A, true)));
        assert_eq!(for_char('Z'), Some(stroke(KeyCode::KEY_Z, true)));
        assert_eq!(for_char('1'), Some(stroke(KeyCode::KEY_1, false)));
        assert_eq!(for_char('0'), Some(stroke(KeyCode::KEY_0, false)));
        assert_eq!(for_char(' '), Some(stroke(KeyCode::KEY_SPACE, false)));
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
        assert_eq!(
            for_key(TerminalKeyCode::Esc),
            Some(stroke(KeyCode::KEY_ESC, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::Enter),
            Some(stroke(KeyCode::KEY_ENTER, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::Backspace),
            Some(stroke(KeyCode::KEY_BACKSPACE, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::Left),
            Some(stroke(KeyCode::KEY_LEFT, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::Down),
            Some(stroke(KeyCode::KEY_DOWN, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::F(1)),
            Some(stroke(KeyCode::KEY_F1, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::F(11)),
            Some(stroke(KeyCode::KEY_F11, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::F(12)),
            Some(stroke(KeyCode::KEY_F12, false))
        );
        assert_eq!(
            for_key(TerminalKeyCode::Modifier(ModifierKeyCode::LeftShift)),
            Some(KeyStroke {
                code: KeyCode::KEY_LEFTSHIFT,
                shift: false,
            })
        );
    }
}
