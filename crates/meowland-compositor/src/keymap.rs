//! Reverse lookup from Kitty key identities to Linux input codes.
//!
//! The compositor owns the configured XKB keymap, so it translates the logical
//! keys a terminal reports into the evdev codes that layout expects. The map
//! covers the first two levels of the active layout and is rebuilt when the
//! active layout index changes; switching between several configured layouts
//! while a key is held is out of scope.

use std::collections::HashMap;

use smithay::input::keyboard::{KeyboardHandle, Layout, xkb};

use super::Compositor;

/// Difference between XKB keycodes and Linux input event codes.
const EVDEV_OFFSET: u32 = 8;
/// First Linux input code the compositor forwards.
const MIN_KEY: u16 = 1;
/// Last Linux input code the compositor forwards.
const MAX_KEY: u16 = 255;
/// Layout levels indexed for the reverse map.
const LEVELS: u32 = 2;
/// First Kitty functional-key number.
const FUNCTIONAL_BASE: u32 = 57344;

/// Reverse keyboard map for the active layout.
#[derive(Debug, Default)]
pub(crate) struct Keymap {
    keysyms: HashMap<xkb::Keysym, u16>,
    layout: Layout,
}

impl Keymap {
    /// Collect the reverse map for the keyboard's active layout.
    pub(crate) fn new(keyboard: &KeyboardHandle<Compositor>, state: &mut Compositor) -> Self {
        let layout = active_layout(keyboard, state);
        Self::build(keyboard, state, layout)
    }

    /// Translate a Kitty key identity through the active layout.
    pub(crate) fn lookup(&self, key: u32) -> Option<u16> {
        if let Some(code) = functional_code(key) {
            return Some(code);
        }
        if key >= FUNCTIONAL_BASE {
            return None;
        }
        let character = char::from_u32(key)?.to_ascii_lowercase();
        if character.is_control() {
            return None;
        }
        let keysym = xkb::utf32_to_keysym(u32::from(character));
        self.keysyms.get(&keysym).copied()
    }

    fn build(
        keyboard: &KeyboardHandle<Compositor>,
        state: &mut Compositor,
        layout: Layout,
    ) -> Self {
        let mut keysyms = HashMap::new();
        keyboard.with_xkb_state(state, |context| {
            let xkb = context.xkb().lock().expect("xkb state lock");
            // SAFETY: the keymap reference is only read inside this closure,
            // which runs while the locked `Xkb` owning it is alive.
            let keymap = unsafe { xkb.keymap() };
            for level in 0..LEVELS {
                keymap.key_for_each(|keymap, keycode| {
                    let Some(code) = key_code(keycode) else {
                        return;
                    };
                    for &keysym in keymap.key_get_syms_by_level(keycode, layout.0, level) {
                        keysyms.entry(keysym).or_insert(code);
                    }
                });
            }
        });
        Self { keysyms, layout }
    }
}

/// Rebuild the reverse map if the keyboard's active layout changed.
pub(crate) fn refresh(state: &mut Compositor) {
    let Some(keyboard) = state.input.keyboard.clone() else {
        return;
    };
    let layout = active_layout(&keyboard, state);
    if layout != state.input.keymap.layout {
        state.input.keymap = Keymap::build(&keyboard, state, layout);
    }
}

/// Text committed for a printable key the active layout cannot type.
pub(crate) fn text(key: u32) -> Option<String> {
    if key >= FUNCTIONAL_BASE {
        return None;
    }
    let character = char::from_u32(key)?;
    (!character.is_control()).then(|| character.to_string())
}

fn active_layout(keyboard: &KeyboardHandle<Compositor>, state: &mut Compositor) -> Layout {
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().expect("xkb state lock");
        xkb.active_layout()
    })
}

/// Linux input code for a Kitty functional key or ASCII control key.
fn functional_code(key: u32) -> Option<u16> {
    Some(match key {
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
        57364..=57373 => u16::try_from(key - 57364 + 59).ok()?,
        57374 => 87,
        57375 => 88,
        57376..=57387 => u16::try_from(key - 57376 + 183).ok()?,
        57399..=57408 => {
            [82, 79, 80, 81, 75, 76, 77, 71, 72, 73][usize::try_from(key - 57399).ok()?]
        }
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
        _ => return None,
    })
}

/// Convert an XKB keycode to the Linux input code the compositor forwards.
fn key_code(keycode: xkb::Keycode) -> Option<u16> {
    let code = keycode.raw().checked_sub(EVDEV_OFFSET)?;
    let code = u16::try_from(code).ok()?;
    (MIN_KEY..=MAX_KEY).contains(&code).then_some(code)
}
