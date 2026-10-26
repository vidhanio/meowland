//! Kitty key identities translated to the compositor's US evdev keymap.
//!
//! Kitty sends logical (unshifted) keys, not Linux scan codes. Keep that
//! translation here; key lifetimes and modifier snapshots are never strokes.

use std::collections::{BTreeSet, HashSet};

use meowland_protocol::{Input, KEY_INSERT, KEY_V, modifiers};

use super::input::{Key, Kind};

/// Held keys and releases intercepted by compositor shortcuts.
#[derive(Debug, Default)]
pub struct Keyboard {
    intercepted: HashSet<u16>,
    held: BTreeSet<u16>,
    modifiers: u8,
}

impl Keyboard {
    pub fn input(&mut self, key: Key) -> Option<Input> {
        self.modifiers = key.modifiers;
        if key.kind == Kind::Repeat {
            return None;
        }
        let code = key_code(key.code)?;
        let pressed = key.kind == Kind::Press;
        if !pressed && self.intercepted.remove(&code) {
            return None;
        }
        let held = self.held.iter().copied().collect();
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        Some(Input::Key {
            code,
            pressed,
            modifiers: key.modifiers,
            held,
        })
    }

    pub fn intercept(&mut self, code: u16) {
        self.held.remove(&code);
        self.intercepted.insert(code);
    }

    #[must_use]
    pub fn enter(&self) -> Input {
        Input::KeyboardEnter {
            keys: self.held.iter().copied().collect(),
            modifiers: self.modifiers,
        }
    }

    pub fn reset(&mut self) {
        self.intercepted.clear();
        self.held.clear();
        self.modifiers = 0;
    }
}

#[must_use]
pub const fn paste(code: u16, bits: u8) -> bool {
    let bits = bits & !(modifiers::CAPS_LOCK | modifiers::NUM_LOCK);
    (code == KEY_V && bits == modifiers::CONTROL)
        || (code == KEY_INSERT && bits == modifiers::SHIFT)
}

pub(crate) fn key_code(code: u32) -> Option<u16> {
    Some(match code {
        27 | 57344 => 1,
        13 | 57345 => 28,
        9 | 57346 => 15,
        8 | 127 | 57347 => 14,
        57348 => 110,
        57349 => 111,
        57350 => 105,
        57351 => 106,
        57352 => 103,
        57353 => 108,
        57354 => 104,
        57355 => 109,
        57356 => 102,
        57357 => 107,
        57358 => 58,
        57359 => 70,
        57360 => 69,
        57361 => 99,
        57362 => 119,
        57363 => 127,
        57364..=57373 => (code - 57364 + 59) as u16,
        57374 => 87,
        57375 => 88,
        57376..=57387 => (code - 57376 + 183) as u16,
        57399..=57408 => [82, 79, 80, 81, 75, 76, 77, 71, 72, 73][(code - 57399) as usize],
        57409 | 57426 => 83,
        57410 => 98,
        57411 => 55,
        57412 => 74,
        57413 => 78,
        57414 => 96,
        57415 => 117,
        57416 => 121,
        57417 => 75,
        57418 => 77,
        57419 => 72,
        57420 => 80,
        57421 => 73,
        57422 => 81,
        57423 => 71,
        57424 => 79,
        57425 => 82,
        57427 => 76,
        57441 => 42,
        57442 => 29,
        57443 => 56,
        57444 => 125,
        57447 => 54,
        57448 => 97,
        57449 => 100,
        57450 => 126,
        57388..=57454 => return None,
        _ => char_code(char::from_u32(code)?)?,
    })
}

const fn char_code(c: char) -> Option<u16> {
    Some(match c.to_ascii_lowercase() {
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
