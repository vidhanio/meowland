//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    desktop::{PopupKind, PopupManager},
    input::{
        keyboard::{FilterResult, KeyboardHandle},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Point, SERIAL_COUNTER},
};

use super::{
    Input, State, apply_window_state,
    render::{extents, hit_test, popup_origin, viewport_of, window_geometry},
};
use crate::protocol::modifiers;

/// Pane modifier bits paired with evdev keycodes.
const MODIFIER_KEYS: [(u8, u16); 4] = [
    (modifiers::SHIFT, 42),
    (modifiers::CONTROL, 29),
    (modifiers::ALT, 56),
    (modifiers::SUPER, 125),
];
pub(super) fn pane_input(state: &mut State, pane: u64, event: &Input) {
    let Some(window) = state.panes.get(&pane).map(|pane| pane.window) else {
        return;
    };
    if window == 0 {
        return;
    }
    state.cursor_pane = Some(pane);
    if state.deciding.insert(window, pane) != Some(pane) {
        apply_window_state(state, window);
    }
    match *event {
        Input::Key {
            code,
            pressed,
            modifiers,
        } => {
            state.focus_window(window);
            if let Some(keyboard) = state.keyboard.take() {
                inject_key(&keyboard, state, code, pressed, modifiers);
                state.keyboard = Some(keyboard);
            }
        }
        Input::Pointer {
            x,
            y,
            button,
            pressed,
            scroll,
        } => {
            if pressed && button.is_some() && !point_in_popups(state, window, x, y) {
                dismiss_popups(state, window);
            }
            let hit = hit_test(state, window, x, y);
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
                if scroll != 0 {
                    pointer.axis(
                        state,
                        AxisFrame::new(time)
                            .source(AxisSource::Wheel)
                            .value(Axis::Vertical, -f64::from(scroll))
                            .v120(Axis::Vertical, -i32::from(scroll.signum()) * 120),
                    );
                }
                pointer.frame(state);
                state.pointer = Some(pointer);
            }
        }
    }
}

/// Popup grabs dismiss on a press outside all popup surfaces.
fn point_in_popups(state: &State, window: u64, x: f64, y: f64) -> bool {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface().clone())
    else {
        return false;
    };
    let geometry = window_geometry(&root);
    PopupManager::popups_for_surface(&root).any(|(popup, location)| {
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

fn inject_key(
    keyboard: &KeyboardHandle<State>,
    state: &mut State,
    code: u16,
    pressed: bool,
    modifiers: u8,
) {
    // Hold modifiers around each stroke; release them in reverse order.
    let time = InputTime::now();
    let emit = |code: u16, pressed: bool, state: &mut State| {
        keyboard.input(
            state,
            Keycode::from(u32::from(code) + 8),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    };
    if pressed {
        for (bit, key) in MODIFIER_KEYS {
            if modifiers & bit != 0 {
                emit(key, true, state);
            }
        }
    }
    emit(code, pressed, state);
    if !pressed {
        for (bit, key) in MODIFIER_KEYS.into_iter().rev() {
            if modifiers & bit != 0 {
                emit(key, false, state);
            }
        }
    }
}
