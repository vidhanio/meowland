//! Translation from terminal key events to Linux key codes.
//!
//! Wayland clients do all key interpretation themselves: they get an xkb keymap
//! plus key *codes* and modifier state. The terminal, on the other hand, hands
//! us characters and modifier flags, already interpreted with the *host*
//! keyboard layout. So the compositor has to run that translation backwards.
//!
//! We do it by pinning the other half of the contract: clients are advertised a
//! plain `us` layout keymap (see [`crate::compositor::Meowland::new`]), and
//! characters are mapped back to the key code and shift state that produce them
//! *in that keymap*. Typing is then layout independent: the terminal decodes
//! the user's physical layout, and we re-encode the resulting character in the
//! keymap we promised the client.
//!
//! Consequence: only characters reachable on a `us` layout can be typed.
//! Everything else (accented letters, emoji, CJK) would need an input method,
//! which the compositor does not implement.

use crossterm::event::{KeyCode, ModifierKeyCode};

/// A key code with the shift state needed to produce the intended symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    /// Linux input event code (`KEY_*`).
    pub code: u32,
    /// Whether shift must be held while it is pressed.
    pub shift: bool,
}

/// Offset between Linux input event codes and the key codes XKB (and therefore
/// the Wayland keyboard protocol) works with.
///
/// Terminals and `KEY_*` constants are evdev; a keymap compiled from the
/// `evdev` rules addresses keys as `evdev + 8`, and that is what has to reach
/// the seat. Getting this wrong does not fail loudly: it types the character
/// eight keys away from the one that was pressed.
pub const XKB_OFFSET: u32 = 8;

const fn stroke(code: u32, shift: bool) -> KeyStroke {
    KeyStroke { code, shift }
}

/// Mouse button codes, spelled the way the Wayland pointer protocol wants them.
pub mod button {
    /// `BTN_LEFT`.
    pub const LEFT: u32 = 0x110;
    /// `BTN_RIGHT`.
    pub const RIGHT: u32 = 0x111;
    /// `BTN_MIDDLE`.
    pub const MIDDLE: u32 = 0x112;
}

/// Key codes for the modifier keys, so clients see real modifier presses rather
/// than only the resulting modifier state.
pub mod modifier {
    /// `KEY_LEFTSHIFT`.
    pub const LEFT_SHIFT: u32 = 42;
    /// `KEY_LEFTSHIFT`'s sibling.
    pub const RIGHT_SHIFT: u32 = 54;
    /// `KEY_LEFTCTRL`.
    pub const LEFT_CTRL: u32 = 29;
    /// `KEY_RIGHTCTRL`.
    pub const RIGHT_CTRL: u32 = 97;
    /// `KEY_LEFTALT`.
    pub const LEFT_ALT: u32 = 56;
    /// `KEY_RIGHTALT`.
    pub const RIGHT_ALT: u32 = 100;
    /// `KEY_LEFTMETA`.
    pub const LEFT_META: u32 = 125;
    /// `KEY_RIGHTMETA`.
    pub const RIGHT_META: u32 = 126;
}

/// Map a character produced by the terminal to the `us` keymap stroke that
/// types it.
pub fn for_char(c: char) -> Option<KeyStroke> {
    if let 'a'..='z' | 'A'..='Z' = c {
        let lowercase = c.to_ascii_lowercase();
        let index = usize::from(lowercase as u8 - b'a');
        return Some(stroke(LETTERS[index], c.is_ascii_uppercase()));
    }

    Some(match c {
        '1'..='9' => stroke(2 + (c as u32 - '1' as u32), false),
        '0' => stroke(11, false),
        // The shifted digit row, in the order the US layout puts the symbols on it.
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

/// The key codes of `F1` to `F12`. The last two are a gap away from the rest,
/// hence a table.
const FUNCTION: [u32; 12] = [59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88];

/// The key codes of `a` to `z` on a `us` keyboard. Not a formula: the letters
/// are laid out in three rows, so the code for a letter is wherever the
/// physical key happens to be.
const LETTERS: [u32; 26] = [
    30, // a
    48, // b
    46, // c
    32, // d
    18, // e
    33, // f
    34, // g
    35, // h
    23, // i
    36, // j
    37, // k
    38, // l
    50, // m
    49, // n
    24, // o
    25, // p
    16, // q
    19, // r
    31, // s
    20, // t
    22, // u
    47, // v
    17, // w
    45, // x
    21, // y
    44, // z
];

/// Map the non-character keys the terminal reports.
pub fn for_key(code: KeyCode) -> Option<KeyStroke> {
    Some(match code {
        KeyCode::Esc => stroke(1, false),
        KeyCode::Enter => stroke(28, false),
        KeyCode::Tab => stroke(15, false),
        KeyCode::BackTab => stroke(15, true),
        KeyCode::Backspace => stroke(14, false),
        KeyCode::Insert => stroke(110, false),
        KeyCode::Delete => stroke(111, false),
        KeyCode::Home => stroke(102, false),
        KeyCode::End => stroke(107, false),
        KeyCode::PageUp => stroke(104, false),
        KeyCode::PageDown => stroke(109, false),
        KeyCode::Up => stroke(103, false),
        KeyCode::Down => stroke(108, false),
        KeyCode::Left => stroke(105, false),
        KeyCode::Right => stroke(106, false),
        KeyCode::CapsLock => stroke(58, false),
        KeyCode::ScrollLock => stroke(70, false),
        KeyCode::NumLock => stroke(69, false),
        KeyCode::PrintScreen => stroke(99, false),
        KeyCode::Pause => stroke(119, false),
        KeyCode::Menu => stroke(127, false),
        KeyCode::F(n) => stroke(*FUNCTION.get(usize::from(n).checked_sub(1)?)?, false),
        KeyCode::Modifier(modifier) => stroke(for_modifier(modifier)?, false),
        KeyCode::Char(c) => return for_char(c),
        _ => return None,
    })
}

/// Map the modifier keys the terminal reports as their own events.
pub const fn for_modifier(modifier: ModifierKeyCode) -> Option<u32> {
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
    use super::*;

    #[test]
    fn shifted_symbols_sit_next_to_their_unshifted_neighbours() {
        // Every pair the US layout shares a key between must agree on the key
        // code, and only the shifted one may require shift: that is
        // what makes typing on the terminal layout independent.
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
        // Nothing in the advertised `us` keymap produces these.
        for c in ['é', '日', '😼', '\u{7f}'] {
            assert_eq!(for_char(c), None);
        }
    }

    #[test]
    fn named_keys_map_to_their_input_codes() {
        assert_eq!(for_key(KeyCode::Esc), Some(stroke(1, false)));
        assert_eq!(for_key(KeyCode::Enter), Some(stroke(28, false)));
        assert_eq!(for_key(KeyCode::Backspace), Some(stroke(14, false)));
        assert_eq!(for_key(KeyCode::Left), Some(stroke(105, false)));
        assert_eq!(for_key(KeyCode::Down), Some(stroke(108, false)));
        assert_eq!(for_key(KeyCode::F(1)), Some(stroke(59, false)));
        assert_eq!(for_key(KeyCode::F(11)), Some(stroke(87, false)));
        assert_eq!(for_key(KeyCode::F(12)), Some(stroke(88, false)));
        assert_eq!(
            for_key(KeyCode::Modifier(ModifierKeyCode::LeftShift)),
            Some(stroke(modifier::LEFT_SHIFT, false))
        );
    }
}
