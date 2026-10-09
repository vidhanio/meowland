//! Kitty key identities translated to the compositor's US evdev keymap.
//!
//! Kitty sends logical (unshifted) keys, not Linux scan codes. Keep that
//! translation here; key lifetimes and modifier snapshots are never strokes.

use std::collections::HashSet;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, ModifierKeyCode,
};

use crate::protocol::{Input, KEY_Q, KEY_W, modifiers};

/// Releases of intercepted shortcuts must stay intercepted even if their
/// modifiers have changed by the time the key comes up.
#[derive(Default)]
pub(super) struct Keyboard {
    intercepted: HashSet<u16>,
}

impl Keyboard {
    pub(super) fn input(&mut self, key: KeyEvent) -> Option<Input> {
        if key.kind == KeyEventKind::Repeat {
            return None;
        }
        let code = key_code(key)?;
        let pressed = key.kind == KeyEventKind::Press;
        if !pressed && self.intercepted.remove(&code) {
            return None;
        }
        Some(Input::Key {
            code,
            pressed,
            modifiers: modifier_bits(key),
        })
    }

    pub(super) fn intercept(&mut self, code: u16) {
        self.intercepted.insert(code);
    }

    pub(super) fn reset(&mut self) {
        self.intercepted.clear();
    }
}

pub(super) const fn binding(code: u16, bits: u8) -> bool {
    modifiers::alt_only(bits) && matches!(code, KEY_Q | KEY_W)
}

pub(super) const fn paste(code: u16, bits: u8) -> bool {
    let bits = bits & !(modifiers::CAPS_LOCK | modifiers::NUM_LOCK);
    (code == 47 && bits == modifiers::CONTROL) || (code == 110 && bits == modifiers::SHIFT)
}

fn modifier_bits(key: KeyEvent) -> u8 {
    let mut bits = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, modifiers::SHIFT),
        (KeyModifiers::CONTROL, modifiers::CONTROL),
        (KeyModifiers::ALT, modifiers::ALT),
        (KeyModifiers::SUPER, modifiers::SUPER),
        (KeyModifiers::HYPER, modifiers::HYPER),
        (KeyModifiers::META, modifiers::META),
    ] {
        if key.modifiers.contains(flag) {
            bits |= bit;
        }
    }
    if key.state.contains(KeyEventState::CAPS_LOCK) {
        bits |= modifiers::CAPS_LOCK;
    }
    if key.state.contains(KeyEventState::NUM_LOCK) {
        bits |= modifiers::NUM_LOCK;
    }
    bits
}

fn key_code(key: KeyEvent) -> Option<u16> {
    if key.state.contains(KeyEventState::KEYPAD) {
        return keypad_code(key.code);
    }
    Some(match key.code {
        KeyCode::Char(c) => char_code(c)?,
        KeyCode::Enter => 28,
        KeyCode::Esc => 1,
        KeyCode::Backspace => 14,
        KeyCode::Tab | KeyCode::BackTab => 15,
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
        KeyCode::CapsLock => 58,
        KeyCode::NumLock => 69,
        KeyCode::ScrollLock => 70,
        KeyCode::PrintScreen => 99,
        KeyCode::Pause => 119,
        KeyCode::Menu => 127,
        KeyCode::F(n) => match n {
            1..=10 => 58 + u16::from(n),
            11 => 87,
            12 => 88,
            13..=24 => 170 + u16::from(n),
            _ => return None,
        },
        KeyCode::Modifier(modifier) => match modifier {
            ModifierKeyCode::LeftShift => 42,
            ModifierKeyCode::RightShift => 54,
            ModifierKeyCode::LeftControl => 29,
            ModifierKeyCode::RightControl => 97,
            ModifierKeyCode::LeftAlt => 56,
            ModifierKeyCode::RightAlt => 100,
            ModifierKeyCode::LeftSuper => 125,
            ModifierKeyCode::RightSuper => 126,
            _ => return None,
        },
        _ => return None,
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

const fn keypad_code(code: KeyCode) -> Option<u16> {
    Some(match code {
        KeyCode::Char('0') | KeyCode::Insert => 82,
        KeyCode::Char('1') | KeyCode::End => 79,
        KeyCode::Char('2') | KeyCode::Down => 80,
        KeyCode::Char('3') | KeyCode::PageDown => 81,
        KeyCode::Char('4') | KeyCode::Left => 75,
        KeyCode::Char('5') | KeyCode::KeypadBegin => 76,
        KeyCode::Char('6') | KeyCode::Right => 77,
        KeyCode::Char('7') | KeyCode::Home => 71,
        KeyCode::Char('8') | KeyCode::Up => 72,
        KeyCode::Char('9') | KeyCode::PageUp => 73,
        KeyCode::Char('.') | KeyCode::Delete => 83,
        KeyCode::Char('/') => 98,
        KeyCode::Char('*') => 55,
        KeyCode::Char('-') => 74,
        KeyCode::Char('+') => 78,
        KeyCode::Enter => 96,
        KeyCode::Char('=') => 117,
        KeyCode::Char(',') => 121,
        _ => return None,
    })
}
