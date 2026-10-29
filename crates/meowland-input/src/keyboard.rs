//! Kitty key identity tracking for the compositor.
//!
//! Kitty sends logical (unshifted) keys, not Linux scan codes, and only the
//! compositor knows the configured XKB layout. Keep key lifetimes and modifier
//! snapshots here and forward the identities untranslated.

use std::collections::{BTreeSet, HashSet};

use meowland_protocol::{Input, KEY_INSERT, KEY_V, Modifiers};

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
        self.modifiers = event.modifiers;
        if event.kind == Kind::Repeat {
            return None;
        }
        let pressed = event.kind == Kind::Press;
        if !pressed && self.intercepted.remove(&event.code) {
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
            pressed,
            modifiers: event.modifiers,
            held,
        })
    }

    /// Consume a shortcut press so its later release is not forwarded to the
    /// application.
    pub fn intercept(&mut self, key: u32) {
        self.held.remove(&key);
        self.intercepted.insert(key);
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
