//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use meowland_protocol::modifiers;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    input::{
        keyboard::{KeyboardHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle},
    },
    reexports::wayland_server::Resource,
    utils::{Point, SERIAL_COUNTER, Serial},
};

use super::{Input, Pane, PaneId, State, WindowId, WlSurface};

const SCROLL_FACTOR: f64 = 0.5;
const SERIAL_HISTORY: usize = 32;

pub fn pane_input(state: &mut State, pane: PaneId, event: &Input) {
    if matches!(event, Input::ResetKeyboard) {
        if state.keyboard_pane == Some(pane) {
            reset_keyboard(state);
        }
        return;
    }
    if matches!(event, Input::Key { pressed: false, .. }) && state.keyboard_pane != Some(pane) {
        return;
    }
    let Some(window) = state.attachments.panes.get(&pane).and_then(Pane::window) else {
        return;
    };
    state.cursor_pane = Some(pane);
    if state.attachments.decide(window, pane) {
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
            let grabbed = state
                .pointer
                .as_ref()
                .is_some_and(PointerHandle::is_grabbed);
            if pressed && button.is_some() && x.is_finite() && y.is_finite() && !grabbed {
                activate_keyboard(state, pane, window);
            }
            let scale = state.scale.factor();
            pointer_input(
                state,
                window,
                (x / scale, y / scale),
                button,
                pressed,
                scroll,
            );
        }
    }
    state.send_pane_cursor(pane);
}

fn pointer_input(
    state: &mut State,
    window: WindowId,
    position: (f64, f64),
    button: Option<u8>,
    pressed: bool,
    scroll: i16,
) {
    let (x, y) = position;
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let root = state
        .windows
        .get(&window)
        .map(|window| window.surface.wl_surface());
    let hit = root.and_then(|root| state.renderer.hit_test(root, &state.snapshots, x, y));
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
            let serial = event_serial(state, pointer.current_focus());
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
                    serial,
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

fn enter_keyboard(state: &mut State, pane: PaneId, window: WindowId, keys: &[u16], bits: u8) {
    reset_keyboard(state);
    if !keyboard_grabbed(state) {
        state.focus_window(window);
    }
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

fn activate_keyboard(state: &mut State, pane: PaneId, window: WindowId) {
    let changed = state.keyboard_pane != Some(pane);
    if changed {
        reset_keyboard(state);
    }
    if !keyboard_grabbed(state) {
        state.focus_window(window);
        if changed && let Some(keyboard) = state.keyboard.take() {
            let surface = state.windows[&window].surface.wl_surface().clone();
            keyboard.set_focus(state, Some(surface), SERIAL_COUNTER.next_serial());
            state.keyboard = Some(keyboard);
        }
    }
    state.keyboard_pane = Some(pane);
}

fn keyboard_grabbed(state: &State) -> bool {
    state
        .keyboard
        .as_ref()
        .is_some_and(KeyboardHandle::is_grabbed)
}

fn event_serial(state: &mut State, focus: Option<WlSurface>) -> Serial {
    let serial = SERIAL_COUNTER.next_serial();
    if let Some(client) = focus.and_then(|surface| surface.client()) {
        state.recent_serials.push_back((serial, client.id()));
        if state.recent_serials.len() > SERIAL_HISTORY {
            state.recent_serials.pop_front();
        }
    }
    serial
}

pub fn valid_serial(state: &State, serial: Serial, surface: &WlSurface) -> bool {
    surface.client().is_some_and(|client| {
        state
            .recent_serials
            .iter()
            .any(|(recent, recipient)| *recent == serial && *recipient == client.id())
    })
}

/// A pane cannot leave keys or snapshot-only modifiers down when it loses
/// focus, disconnects, or starts showing a different window.
pub fn reset_keyboard(state: &mut State) {
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
    let serial = event_serial(state, keyboard.current_focus());
    keyboard.input_forward(
        state,
        Keycode::from(u32::from(code) + 8),
        if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
        serial,
        InputTime::now(),
        false,
    );
}

/// A terminal paste action has no physical key event. Its explicit shortcut
/// leaves the real held-key set and modifier state unchanged.
pub fn paste_action(state: &mut State, pane: PaneId) {
    let Some(window) = state.attachments.panes.get(&pane).and_then(Pane::window) else {
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
