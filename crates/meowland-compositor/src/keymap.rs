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
/// First key number after the Kitty functional-key range.
const FUNCTIONAL_END: u32 = 63744;

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
        if is_functional(key) {
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
    if is_functional(key) {
        return None;
    }
    let character = char::from_u32(key)?;
    (!character.is_control()).then(|| character.to_string())
}

fn is_functional(key: u32) -> bool {
    (FUNCTIONAL_BASE..FUNCTIONAL_END).contains(&key)
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

#[cfg(test)]
mod tests {
    use smithay::input::keyboard::xkb;

    use super::*;

    #[test]
    fn functional_code_covers_key_ranges_and_rejects_gaps() {
        let cases = [
            (27, 1),
            (13, 28),
            (9, 15),
            (8, 14),
            (127, 14),
            (57344, 1),
            (57352, 103),
            (57353, 108),
            (57364, 59),
            (57373, 68),
            (57374, 87),
            (57375, 88),
            (57376, 183),
            (57387, 194),
            (57399, 82),
            (57408, 73),
            (57409, 83),
            (57410, 98),
            (57426, 83),
            (57427, 76),
            (57441, 42),
            (57450, 126),
        ];
        for (key, expected) in cases {
            assert_eq!(functional_code(key), Some(expected), "key {key}");
        }

        for key in [0, 1, 97, 57343, 57428, 57440, 57451, 0x11_0000] {
            assert_eq!(functional_code(key), None, "key {key}");
        }
    }

    #[test]
    fn text_skips_controls_and_functional_keys() {
        assert_eq!(text(u32::from('a')), Some("a".to_owned()));
        assert_eq!(text(u32::from('é')), Some("é".to_owned()));
        assert_eq!(text(0x1f600), Some("😀".to_owned()));
        assert_eq!(text(63743), None);
        assert!(text(63744).is_some());

        for key in [0, 9, 27, 0x7f, 0x80, 57344, 0xd800, 0x11_0000] {
            assert_eq!(text(key), None, "key {key}");
        }
    }

    #[test]
    fn lookup_prefers_functional_keys_then_lowercased_layout_keysyms() {
        let empty = Keymap::default();
        assert_eq!(empty.lookup(57344), Some(1));
        assert_eq!(empty.lookup(27), Some(1));
        assert_eq!(empty.lookup(1), None);
        assert_eq!(empty.lookup(57343), None);
        assert_eq!(empty.lookup(0x11_0000), None);
        assert_eq!(empty.lookup(u32::from('q')), None);

        let mut keysyms = HashMap::new();
        keysyms.insert(xkb::utf32_to_keysym(u32::from('q')), 16);
        keysyms.insert(xkb::utf32_to_keysym(u32::from('é')), 18);
        keysyms.insert(xkb::utf32_to_keysym(0x1f600), 19);
        keysyms.insert(xkb::utf32_to_keysym(63743), 20);
        keysyms.insert(xkb::utf32_to_keysym(63744), 21);
        let keymap = Keymap {
            keysyms,
            layout: Layout::default(),
        };
        assert_eq!(keymap.lookup(u32::from('Q')), Some(16));
        assert_eq!(keymap.lookup(u32::from('q')), Some(16));
        assert_eq!(keymap.lookup(u32::from('é')), Some(18));
        assert_eq!(keymap.lookup(0x1f600), Some(19));
        assert_eq!(keymap.lookup(63743), None);
        assert_eq!(keymap.lookup(63744), Some(21));
        assert_eq!(keymap.lookup(u32::from('a')), None);
    }

    #[test]
    fn key_code_applies_the_evdev_offset_and_bounds() {
        assert_eq!(key_code(xkb::Keycode::new(9)), Some(1));
        assert_eq!(key_code(xkb::Keycode::new(263)), Some(255));
        assert_eq!(key_code(xkb::Keycode::new(8)), None);
        assert_eq!(key_code(xkb::Keycode::new(264)), None);
        assert_eq!(key_code(xkb::Keycode::new(u32::MAX)), None);
    }
}
