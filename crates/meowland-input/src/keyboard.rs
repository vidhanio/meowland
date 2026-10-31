//! Kitty key identity tracking for the compositor.
//!
//! Kitty sends logical (unshifted) keys, not Linux scan codes, and only the
//! compositor knows the configured XKB layout. Keep key lifetimes and modifier
//! snapshots here and forward the identities untranslated.

use std::collections::{BTreeSet, HashSet};

use meowland_protocol::{
    Input, KEY_INSERT, KEY_V, MAX_HELD_KEYS, Modifiers, Transition, valid_key_identity,
};

use super::input::{Key, Kind};

/// Held keys and releases intercepted by compositor shortcuts.
#[derive(Debug, Default)]
pub struct Keyboard {
    intercepted: HashSet<u32>,
    held: BTreeSet<u32>,
    modifiers: Modifiers,
}

impl Keyboard {
    /// Retain held keys and suppress intercepted releases or repeats.
    pub fn input(&mut self, event: Key) -> Option<Input> {
        if !valid_key_identity(event.code) {
            return None;
        }
        self.modifiers = event.modifiers;
        if event.kind == Kind::Repeat
            || self.intercepted.contains(&event.code) && event.kind == Kind::Press
        {
            return None;
        }
        let pressed = event.kind == Kind::Press;
        if !pressed {
            if self.intercepted.remove(&event.code) || !self.held.contains(&event.code) {
                return None;
            }
        } else if !self.held.contains(&event.code)
            && self.held.len() + self.intercepted.len() >= MAX_HELD_KEYS
        {
            return None;
        }
        let held = self.held.iter().copied().collect();
        if pressed {
            self.held.insert(event.code);
        } else {
            self.held.remove(&event.code);
        }
        Some(Input::Key {
            key: event.code,
            state: if pressed {
                Transition::Pressed
            } else {
                Transition::Released
            },
            modifiers: event.modifiers,
            text: event.text,
            held,
        })
    }

    /// Consume a shortcut press so its later release is not forwarded to the
    /// application.
    pub fn intercept(&mut self, key: u32) {
        if self.held.remove(&key) {
            self.intercepted.insert(key);
        }
    }

    /// Snapshot held keys and modifiers for a newly focused compositor pane.
    #[must_use]
    pub fn enter(&self) -> Input {
        Input::KeyboardEnter {
            keys: self.held.iter().copied().collect(),
            modifiers: self.modifiers,
        }
    }

    /// Forget held keys, intercepted releases, and modifiers after focus loss.
    pub fn reset(&mut self) {
        self.intercepted.clear();
        self.held.clear();
        self.modifiers = Modifiers::empty();
    }
}

/// Recognize Control-V or Shift-Insert, ignoring lock modifiers.
#[must_use]
pub const fn paste(key: u32, bits: Modifiers) -> bool {
    let bits = bits.difference(Modifiers::CAPS_LOCK.union(Modifiers::NUM_LOCK));
    (key == KEY_V && bits.bits() == Modifiers::CONTROL.bits())
        || (key == KEY_INSERT && bits.bits() == Modifiers::SHIFT.bits())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u32, kind: Kind) -> Key {
        Key {
            code,
            kind,
            modifiers: Modifiers::empty(),
            text: None,
        }
    }

    #[test]
    fn invalid_and_overflowing_keys_do_not_poison_snapshots() {
        let mut keyboard = Keyboard::default();
        assert!(keyboard.input(key(0x0011_0000, Kind::Press)).is_none());
        for code in 1..=u32::try_from(MAX_HELD_KEYS).expect("limit fits u32") {
            let Some(Input::Key { held, .. }) = keyboard.input(key(code, Kind::Press)) else {
                panic!("valid press {code} was dropped");
            };
            assert_eq!(
                held.len(),
                usize::try_from(code).expect("key fits usize") - 1
            );
        }
        assert!(keyboard.input(key(1000, Kind::Press)).is_none());
        assert!(keyboard.input(key(1000, Kind::Release)).is_none());
        let Some(Input::Key { held, .. }) = keyboard.input(key(1, Kind::Release)) else {
            panic!("tracked release was dropped");
        };
        assert_eq!(held.len(), MAX_HELD_KEYS);
        assert!(keyboard.input(key(1000, Kind::Press)).is_some());
        let Input::KeyboardEnter { keys, .. } = keyboard.enter() else {
            unreachable!()
        };
        assert_eq!(keys.len(), MAX_HELD_KEYS);
        assert!(!keys.contains(&1));
        assert!(keys.contains(&1000));
        keyboard.reset();
        assert!(keyboard.input(key(1000, Kind::Release)).is_none());
        assert!(matches!(keyboard.enter(), Input::KeyboardEnter { keys, .. } if keys.is_empty()));
        assert!(keyboard.input(key(65, Kind::Press)).is_some());
    }

    #[test]
    fn intercepted_keys_share_the_tracking_limit() {
        let mut keyboard = Keyboard::default();
        for code in 1..=u32::try_from(MAX_HELD_KEYS).expect("limit fits u32") {
            assert!(keyboard.input(key(code, Kind::Press)).is_some());
            keyboard.intercept(code);
        }
        assert!(keyboard.input(key(1000, Kind::Press)).is_none());
        assert!(keyboard.input(key(1, Kind::Release)).is_none());
        assert!(keyboard.input(key(1000, Kind::Press)).is_some());
        keyboard.reset();
        assert!(keyboard.input(key(2, Kind::Release)).is_none());
    }
}
