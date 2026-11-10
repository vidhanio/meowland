//! Kitty key identity tracking for the compositor.
//!
//! Kitty sends logical (unshifted) keys, not Linux scan codes, and only the
//! compositor knows the configured XKB layout. Keep key lifetimes and modifier
//! snapshots here and forward the identities untranslated.

use std::collections::BTreeMap;

use meowland_protocol::{Input, MAX_HELD_KEYS, Modifiers, Transition, valid_key_identity};

use super::input::{Key, Kind};

/// What happens to a held key's later events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// The press was forwarded; its release will be too.
    Forwarded,
    /// A compositor shortcut consumed the press; repeats and the release are
    /// suppressed.
    Intercepted,
}

/// Held keys and releases intercepted by compositor shortcuts.
#[derive(Debug, Default)]
pub struct Keyboard {
    keys: BTreeMap<u32, Held>,
    modifiers: Modifiers,
}

impl Keyboard {
    /// Retain held keys and suppress intercepted releases or repeats.
    pub fn input(&mut self, event: Key) -> Option<Input> {
        if !valid_key_identity(event.code) {
            return None;
        }
        self.modifiers = event.modifiers;
        let state = match (event.kind, self.keys.get(&event.code)) {
            (Kind::Press, Some(Held::Forwarded)) => Transition::Pressed,
            (Kind::Press, None) if self.keys.len() < MAX_HELD_KEYS => Transition::Pressed,
            (Kind::Release, Some(Held::Forwarded)) => Transition::Released,
            (Kind::Release, Some(Held::Intercepted)) => {
                self.keys.remove(&event.code);
                return None;
            }
            _ => return None,
        };
        let held = self.forwarded();
        match state {
            Transition::Pressed => self.keys.insert(event.code, Held::Forwarded),
            Transition::Released => self.keys.remove(&event.code),
        };
        Some(Input::Key {
            key: event.code,
            state,
            modifiers: event.modifiers,
            text: event.text,
            held,
        })
    }

    fn forwarded(&self) -> Vec<u32> {
        self.keys
            .iter()
            .filter(|&(_, &held)| held == Held::Forwarded)
            .map(|(&code, _)| code)
            .collect()
    }

    /// Consume a shortcut press so its later release is not forwarded to the
    /// application.
    pub fn intercept(&mut self, key: u32) {
        if let Some(held) = self.keys.get_mut(&key) {
            *held = Held::Intercepted;
        }
    }

    /// Snapshot held keys and modifiers for a newly focused compositor pane.
    #[must_use]
    pub fn enter(&self) -> Input {
        Input::KeyboardEnter {
            keys: self.forwarded(),
            modifiers: self.modifiers,
        }
    }

    /// Forget held keys, intercepted releases, and modifiers after focus loss.
    pub fn reset(&mut self) {
        self.keys.clear();
        self.modifiers = Modifiers::empty();
    }
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
            panic!("keyboard enter produced another input");
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
