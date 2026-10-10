//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    desktop::{PopupKind, PopupManager},
    input::{
        keyboard::{KeyboardHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Point, SERIAL_COUNTER},
};

use super::{
    Input, State,
    render::{extents, popup_origin, viewport_of, window_geometry},
};
use crate::protocol::modifiers;

const SCROLL_FACTOR: f64 = 0.5;

pub(super) fn pane_input(state: &mut State, pane: u64, event: &Input) {
    if matches!(event, Input::ResetKeyboard) {
        if state.keyboard_pane == Some(pane) {
            reset_keyboard(state);
        }
        return;
    }
    if matches!(event, Input::Key { pressed: false, .. }) && state.keyboard_pane != Some(pane) {
        return;
    }
    let Some(window) = state.panes.get(&pane).map(|pane| pane.window) else {
        return;
    };
    if window == 0 {
        return;
    }
    state.cursor_pane = Some(pane);
    if state.deciding.insert(window, pane) != Some(pane) {
        state.apply_window_state(window);
    }
    match *event {
        Input::Key {
            code,
            pressed,
            modifiers,
            ref held,
        } => {
            if held.len() > 256 || held.iter().any(|code| *code == 0 || *code > 255) {
                return;
            }
            if state.keyboard_pane != Some(pane) || !state.keyboard_known {
                enter_keyboard(state, pane, window, held, modifiers);
            } else {
                activate_keyboard(state, pane, window);
            }
            if let Some(keyboard) = state.keyboard.take() {
                inject_key(&keyboard, state, code, pressed, modifiers);
                state.keyboard = Some(keyboard);
            }
        }
        Input::KeyboardEnter {
            ref keys,
            modifiers,
        } => {
            if keys.len() > 256 || keys.iter().any(|code| *code == 0 || *code > 255) {
                return;
            }
            enter_keyboard(state, pane, window, keys, modifiers);
        }
        Input::ResetKeyboard => unreachable!(),
        Input::Pointer {
            x,
            y,
            button,
            pressed,
            scroll,
        } => {
            if pressed && button.is_some() && x.is_finite() && y.is_finite() {
                activate_keyboard(state, pane, window);
            }
            pointer_input(state, window, (x, y), button, pressed, scroll);
        }
    }
}

fn pointer_input(
    state: &mut State,
    window: u64,
    position: (f64, f64),
    button: Option<u8>,
    pressed: bool,
    scroll: i16,
) {
    let (x, y) = position;
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    if pressed && button.is_some() && !point_in_popups(state, window, x, y) {
        dismiss_popups(state, window);
    }
    let hit = state.hit_test(window, x, y);
    let location = Point::from((x, y));
    if let Some(pointer) = state.pointer.take() {
        let time = InputTime::now();
        pointer.motion(
            state,
            hit,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        if let Some(button) = button {
            pointer.button(
                state,
                &ButtonEvent {
                    button: match button {
                        0 => 0x110,
                        1 => 0x111,
                        2 => 0x112,
                        _ => 0x113,
                    },
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        let amount = -f64::from(scroll) * SCROLL_FACTOR;
        if amount != 0.0 {
            pointer.frame(state);
            pointer.axis(
                state,
                AxisFrame::new(time)
                    .source(AxisSource::Wheel)
                    .value(Axis::Vertical, amount * (15.0 / 120.0))
                    .v120(Axis::Vertical, amount as i32),
            );
        }
        pointer.frame(state);
        state.pointer = Some(pointer);
    }
}

/// Popup grabs dismiss on a press outside all popup surfaces.
fn point_in_popups(state: &State, window: u64, x: f64, y: f64) -> bool {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface())
    else {
        return false;
    };
    let geometry = window_geometry(root);
    PopupManager::popups_for_surface(root).any(|(popup, location)| {
        let surface = popup.wl_surface();
        let Some(snapshot) = state.snapshots.get(surface) else {
            return false;
        };
        let (_, dst) = extents(snapshot, viewport_of(surface));
        let origin = popup_origin(geometry, location, &popup);
        let left = f64::from(origin.x);
        let top = f64::from(origin.y);
        x >= left && y >= top && x < left + f64::from(dst.w) && y < top + f64::from(dst.h)
    })
}

fn dismiss_popups(state: &mut State, window: u64) {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface().clone())
    else {
        return;
    };
    let popups: Vec<PopupKind> = PopupManager::popups_for_surface(&root)
        .map(|(popup, _)| popup)
        .collect();
    if popups.is_empty() {
        return;
    }
    for popup in popups.into_iter().rev() {
        if let PopupKind::Xdg(popup) = &popup {
            popup.send_popup_done();
        }
        let _ = PopupManager::dismiss_popup(&root, &popup);
    }
    state.touch(window);
}

fn enter_keyboard(state: &mut State, pane: u64, window: u64, keys: &[u16], bits: u8) {
    reset_keyboard(state);
    state.focus_window(window);
    if let Some(keyboard) = state.keyboard.take() {
        keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
        set_modifiers(&keyboard, state, snapshot(bits));
        for code in keys {
            if state.keyboard_keys.insert(*code) {
                forward(&keyboard, state, *code, true);
            }
        }
        let surface = state.windows[&window].surface.wl_surface().clone();
        keyboard.set_focus(state, Some(surface), SERIAL_COUNTER.next_serial());
        state.keyboard = Some(keyboard);
    }
    state.keyboard_pane = Some(pane);
    state.keyboard_known = true;
}

fn activate_keyboard(state: &mut State, pane: u64, window: u64) {
    let changed = state.keyboard_pane != Some(pane);
    if changed {
        reset_keyboard(state);
    }
    state.focus_window(window);
    if changed && let Some(keyboard) = state.keyboard.take() {
        let surface = state.windows[&window].surface.wl_surface().clone();
        keyboard.set_focus(state, Some(surface), SERIAL_COUNTER.next_serial());
        state.keyboard = Some(keyboard);
    }
    state.keyboard_pane = Some(pane);
}

/// A pane cannot leave keys or snapshot-only modifiers down when it loses
/// focus, disconnects, or starts showing a different window.
pub(super) fn reset_keyboard(state: &mut State) {
    state.keyboard_pane = None;
    state.keyboard_known = false;
    let keys = std::mem::take(&mut state.keyboard_keys);
    if let Some(keyboard) = state.keyboard.take() {
        set_modifiers(&keyboard, state, ModifiersState::default());
        for code in keys {
            forward(&keyboard, state, code, false);
        }
        keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
        state.keyboard = Some(keyboard);
    }
}

fn snapshot(bits: u8) -> ModifiersState {
    ModifiersState {
        shift: bits & modifiers::SHIFT != 0,
        ctrl: bits & modifiers::CONTROL != 0,
        alt: bits & modifiers::ALT != 0,
        logo: bits & modifiers::SUPER != 0,
        caps_lock: bits & modifiers::CAPS_LOCK != 0,
        num_lock: bits & modifiers::NUM_LOCK != 0,
        ..ModifiersState::default()
    }
}

fn set_modifiers(keyboard: &KeyboardHandle<State>, state: &mut State, modifiers: ModifiersState) {
    if keyboard.set_modifier_state(modifiers) != 0 {
        keyboard.advertise_modifier_state(state);
    }
}

/// The terminal is the modifier authority, like a parent Wayland seat. Do not
/// re-derive its state by feeding logical key identities through XKB again.
fn inject_key(
    keyboard: &KeyboardHandle<State>,
    state: &mut State,
    code: u16,
    pressed: bool,
    bits: u8,
) {
    if code == 0 || code > 255 {
        return;
    }
    set_modifiers(keyboard, state, snapshot(bits));
    let changed = if pressed {
        state.keyboard_keys.insert(code)
    } else {
        state.keyboard_keys.remove(&code)
    };
    if changed {
        forward(keyboard, state, code, pressed);
    }
}

fn forward(keyboard: &KeyboardHandle<State>, state: &mut State, code: u16, pressed: bool) {
    keyboard.input_forward(
        state,
        Keycode::from(u32::from(code) + 8),
        if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
        SERIAL_COUNTER.next_serial(),
        InputTime::now(),
        false,
    );
}

/// A terminal paste action has no physical key event. Its explicit shortcut
/// leaves the real held-key set and modifier state unchanged.
pub(super) fn paste_action(state: &mut State, pane: u64) {
    let Some(window) = state
        .panes
        .get(&pane)
        .map(|pane| pane.window)
        .filter(|window| *window != 0)
    else {
        return;
    };
    activate_keyboard(state, pane, window);
    if state.keyboard_keys.contains(&47) {
        return;
    }
    if let Some(keyboard) = state.keyboard.take() {
        let previous = keyboard.modifier_state();
        set_modifiers(&keyboard, state, snapshot(modifiers::CONTROL));
        forward(&keyboard, state, 47, true);
        forward(&keyboard, state, 47, false);
        set_modifiers(&keyboard, state, previous);
        state.keyboard = Some(keyboard);
    }
}
