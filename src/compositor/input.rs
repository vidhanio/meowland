//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    desktop::{PopupKind, PopupManager},
    input::{
        keyboard::{FilterResult, KeyboardHandle, KeyboardSource, ModifiersState},
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
        } => {
            activate_keyboard(state, pane, window);
            let source = state.panes[&pane].keyboard_source;
            if let Some(keyboard) = state.keyboard.take() {
                inject_key(&keyboard, state, source, code, pressed, modifiers);
                state.keyboard = Some(keyboard);
            }
        }
        Input::KeyTap { code, modifiers } => {
            activate_keyboard(state, pane, window);
            if let Some(keyboard) = state.keyboard.take() {
                tap_key(&keyboard, state, code, modifiers);
                state.keyboard = Some(keyboard);
            }
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

fn activate_keyboard(state: &mut State, pane: u64, window: u64) {
    if state.keyboard_pane != Some(pane) {
        reset_keyboard(state);
    }
    state.focus_window(window);
    state.keyboard_pane = Some(pane);
}

/// A pane cannot leave keys or snapshot-only modifiers down when it loses
/// focus, disconnects, or starts showing a different window.
pub(super) fn reset_keyboard(state: &mut State) {
    let pane = state.keyboard_pane.take();
    if let Some(keyboard) = state.keyboard.take() {
        if let Some(source) =
            pane.and_then(|pane| state.panes.get(&pane).map(|p| p.keyboard_source))
        {
            keyboard.release_source(state, source);
        }
        set_modifiers(&keyboard, state, ModifiersState::default());
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

fn inject_key(
    keyboard: &KeyboardHandle<State>,
    state: &mut State,
    source: KeyboardSource,
    code: u16,
    pressed: bool,
    bits: u8,
) {
    let before = keyboard.modifier_state();
    let keycode = Keycode::from(u32::from(code) + 8);
    let serial = SERIAL_COUNTER.next_serial();
    let time = InputTime::now();
    let direction = if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    };
    if keyboard
        .input_from_source(
            source,
            state,
            keycode,
            direction,
            serial,
            time,
            |_, _, _| FilterResult::Intercept(()),
        )
        .is_none()
    {
        return;
    }
    let mut modifiers = snapshot(bits);
    let held = |left: u32, right: u32| {
        let keys = keyboard.pressed_keys();
        keys.contains(&Keycode::from(left + 8)) || keys.contains(&Keycode::from(right + 8))
    };
    match code {
        42 | 54 => modifiers.shift = held(42, 54),
        29 | 97 => modifiers.ctrl = held(29, 97),
        56 | 100 => modifiers.alt = held(56, 100),
        125 | 126 => modifiers.logo = held(125, 126),
        _ => {}
    }
    keyboard.set_modifier_state(modifiers);
    if before != keyboard.modifier_state() {
        keyboard.advertise_modifier_state(state);
    }
    keyboard.input_forward(state, keycode, direction, serial, time, false);
}

/// Only clipboard commands are taps. Keep them separate from real input and
/// restore the held modifier state instead of releasing the user's modifiers.
fn tap_key(keyboard: &KeyboardHandle<State>, state: &mut State, code: u16, bits: u8) {
    let previous = keyboard.modifier_state();
    set_modifiers(keyboard, state, snapshot(bits));
    let source = KeyboardSource::new_auxiliary();
    for direction in [KeyState::Pressed, KeyState::Released] {
        keyboard.input_from_source(
            source,
            state,
            Keycode::from(u32::from(code) + 8),
            direction,
            SERIAL_COUNTER.next_serial(),
            InputTime::now(),
            |_, _, _| FilterResult::<()>::Forward,
        );
    }
    set_modifiers(keyboard, state, previous);
}
