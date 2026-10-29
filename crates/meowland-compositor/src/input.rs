//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use std::collections::{HashSet, VecDeque};

use meowland_protocol::{Button, ButtonState, Input, KEY_V, Scroll, modifiers};
use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState as WaylandButtonState, InputTime, KeyState, Keycode,
    },
    input::{
        keyboard::{KeyboardHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle},
    },
    reexports::wayland_server::{Resource, backend::ClientId},
    utils::{Point, SERIAL_COUNTER, Serial},
};

use super::{AttachedPane, PaneId, State, WindowId, WlSurface, text};

#[derive(Default)]
pub(crate) struct KeyboardState {
    pub owner: Option<PaneId>,
    pub held: HashSet<u16>,
    pub known: bool,
    pub serials: VecDeque<(Serial, ClientId)>,
}

const SERIAL_HISTORY: usize = 32;
/// Linux input code for the primary pointer button.
const BTN_LEFT: u32 = 0x110;
/// Linux input code for the secondary pointer button.
const BTN_RIGHT: u32 = 0x111;
/// Linux input code for the middle pointer button.
const BTN_MIDDLE: u32 = 0x112;
/// Linux input code for the backward side button.
const BTN_SIDE: u32 = 0x113;
/// Linux input code for the forward side button.
const BTN_EXTRA: u32 = 0x114;
/// First Linux input code reserved for buttons without a named code.
const BTN_TRIGGER_HAPPY1: u32 = 0x2c0;
/// Number of Linux input codes reserved for buttons without a named code.
const EXTRA_BUTTON_CODES: u8 = 40;
/// First X11 button number without a named protocol [`Button`] variant.
const FIRST_EXTRA_BUTTON: u8 = 10;
/// Wheel units the terminal reports for one detent.
const SCROLL_UNITS_PER_DETENT: i32 = 120;
/// Wayland axis distance emitted for one wheel detent.
const AXIS_UNITS_PER_DETENT: f64 = 7.5;
/// High-resolution Wayland units emitted for one wheel detent.
const V120_PER_DETENT: i32 = 60;

pub(crate) fn pane_input(state: &mut State, pane: PaneId, event: &Input) {
    if !valid_input(event) {
        return;
    }
    state.refresh_popup_grab();
    if matches!(event, Input::ResetKeyboard) {
        if state.keyboard_state.owner == Some(pane) {
            reset_keyboard(state);
        }
        return;
    }
    if matches!(event, Input::Key { pressed: false, .. })
        && state.keyboard_state.owner != Some(pane)
    {
        return;
    }
    let Some(window) = state.attachments.get(pane).and_then(AttachedPane::window) else {
        return;
    };
    if !state.catalog.drawable(window) {
        return;
    }
    state.last_input_pane = Some(pane);
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
            if state.keyboard_state.owner != Some(pane) || !state.keyboard_state.known {
                enter_keyboard(state, pane, window, held, modifiers);
            } else {
                activate_keyboard(state, pane, window);
            }
            if let Some(keyboard) = state.keyboard.take() {
                let transition = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                inject_key(&keyboard, state, code, transition, modifiers);
                state.keyboard = Some(keyboard);
            }
        }
        Input::KeyboardEnter {
            ref keys,
            modifiers,
        } => {
            enter_keyboard(state, pane, window, keys, modifiers);
        }
        Input::ResetKeyboard => unreachable!(),
        Input::Text(ref text) => {
            activate_keyboard(state, pane, window);
            text::commit(state, text);
        }
        Input::Pointer {
            x,
            y,
            button,
            scroll,
        } => {
            let grabbed = state
                .pointer
                .as_ref()
                .is_some_and(PointerHandle::is_grabbed);
            if !grabbed && button.is_some_and(|(_, transition)| transition == ButtonState::Pressed)
            {
                activate_keyboard(state, pane, window);
            }
            let scale = state.scale.factor();
            pointer_input(state, window, (x / scale, y / scale), button, scroll);
        }
    }
    state.send_pane_cursor(pane);
}

fn valid_input(event: &Input) -> bool {
    match event {
        Input::Key { code, held, .. } => valid_key(*code) && valid_keys(held),
        Input::KeyboardEnter { keys, .. } => valid_keys(keys),
        Input::Pointer { x, y, .. } => x.is_finite() && y.is_finite(),
        Input::Text(_) | Input::ResetKeyboard => true,
    }
}

const fn valid_key(code: u16) -> bool {
    code > 0 && code <= 255
}

fn valid_keys(keys: &[u16]) -> bool {
    keys.len() <= 256 && keys.iter().all(|code| valid_key(*code))
}

fn pointer_input(
    state: &mut State,
    window: WindowId,
    position: (f64, f64),
    button: Option<(Button, ButtonState)>,
    scroll: Scroll,
) {
    let (x, y) = position;
    let root = state
        .windows
        .get(&window)
        .map(|window| window.surface.wl_surface());
    let hit = root.and_then(|root| {
        state
            .renderer
            .hit_test(root, &state.surface_cache.snapshots, x, y)
    });
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
        if let Some((button, transition)) = button {
            let serial = event_serial(state, pointer.current_focus());
            pointer.button(
                state,
                &ButtonEvent {
                    button: button_code(button),
                    state: match transition {
                        ButtonState::Pressed => WaylandButtonState::Pressed,
                        ButtonState::Released => WaylandButtonState::Released,
                    },
                    serial,
                    time,
                },
            );
        }
        let vertical = -i32::from(scroll.vertical);
        let horizontal = i32::from(scroll.horizontal);
        if vertical != 0 || horizontal != 0 {
            let frame = AxisFrame::new(time).source(AxisSource::Wheel);
            let frame = if vertical == 0 {
                frame
            } else {
                frame
                    .value(Axis::Vertical, axis_units(vertical))
                    .v120(Axis::Vertical, v120_units(vertical))
            };
            let frame = if horizontal == 0 {
                frame
            } else {
                frame
                    .value(Axis::Horizontal, axis_units(horizontal))
                    .v120(Axis::Horizontal, v120_units(horizontal))
            };
            pointer.frame(state);
            pointer.axis(state, frame);
        }
        pointer.frame(state);
        state.pointer = Some(pointer);
    }
}

/// Map a terminal pointer button to its Linux input code.
fn button_code(button: Button) -> u32 {
    match button {
        Button::Left => BTN_LEFT,
        Button::Middle => BTN_MIDDLE,
        Button::Right => BTN_RIGHT,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
        Button::Other(number) => {
            let index = number.saturating_sub(FIRST_EXTRA_BUTTON);
            let index = if index < EXTRA_BUTTON_CODES {
                index
            } else {
                EXTRA_BUTTON_CODES - 1
            };
            BTN_TRIGGER_HAPPY1 + u32::from(index)
        }
    }
}

/// Convert a wheel delta in terminal units to Wayland axis distance.
fn axis_units(units: i32) -> f64 {
    f64::from(units) * AXIS_UNITS_PER_DETENT / f64::from(SCROLL_UNITS_PER_DETENT)
}

/// Convert a wheel delta in terminal units to Wayland high-resolution units.
const fn v120_units(units: i32) -> i32 {
    units * V120_PER_DETENT / SCROLL_UNITS_PER_DETENT
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
            if state.keyboard_state.held.insert(*code) {
                forward(&keyboard, state, *code, KeyState::Pressed);
            }
        }
        let surface = state.windows[&window].surface.wl_surface().clone();
        keyboard.set_focus(state, Some(surface), SERIAL_COUNTER.next_serial());
        state.keyboard = Some(keyboard);
    }
    state.keyboard_state.owner = Some(pane);
    state.keyboard_state.known = true;
}

fn activate_keyboard(state: &mut State, pane: PaneId, window: WindowId) {
    let changed = state.keyboard_state.owner != Some(pane);
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
    state.keyboard_state.owner = Some(pane);
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
        state
            .keyboard_state
            .serials
            .push_back((serial, client.id()));
        if state.keyboard_state.serials.len() > SERIAL_HISTORY {
            state.keyboard_state.serials.pop_front();
        }
    }
    serial
}

pub(crate) fn valid_serial(state: &State, serial: Serial, surface: &WlSurface) -> bool {
    surface.client().is_some_and(|client| {
        let keyboard = state
            .keyboard
            .as_ref()
            .and_then(KeyboardHandle::current_focus);
        let pointer = state
            .pointer
            .as_ref()
            .and_then(PointerHandle::current_focus);
        let focused = keyboard.into_iter().chain(pointer).any(|focus| {
            focus
                .client()
                .is_some_and(|focused| focused.id() == client.id())
        });
        focused
            && state
                .keyboard_state
                .serials
                .iter()
                .any(|(recent, recipient)| *recent == serial && *recipient == client.id())
    })
}

/// A pane cannot leave keys or snapshot-only modifiers down when it loses
/// focus, disconnects, or starts showing a different window.
pub(crate) fn reset_keyboard(state: &mut State) {
    state.keyboard_state.owner = None;
    state.keyboard_state.known = false;
    let keys = std::mem::take(&mut state.keyboard_state.held);
    if let Some(keyboard) = state.keyboard.take() {
        set_modifiers(&keyboard, state, ModifiersState::default());
        for code in keys {
            forward(&keyboard, state, code, KeyState::Released);
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
    transition: KeyState,
    bits: u8,
) {
    set_modifiers(keyboard, state, snapshot(bits));
    let changed = if transition == KeyState::Pressed {
        state.keyboard_state.held.insert(code)
    } else {
        state.keyboard_state.held.remove(&code)
    };
    if changed {
        forward(keyboard, state, code, transition);
    }
}

fn forward(keyboard: &KeyboardHandle<State>, state: &mut State, code: u16, transition: KeyState) {
    let serial = event_serial(state, keyboard.current_focus());
    keyboard.input_forward(
        state,
        Keycode::from(u32::from(code) + 8),
        transition,
        serial,
        InputTime::now(),
        false,
    );
}

/// A terminal paste action has no physical key event. Its explicit shortcut
/// leaves the real held-key set and modifier state unchanged.
pub(crate) fn paste_action(state: &mut State, pane: PaneId) {
    let Some(window) = state.attachments.get(pane).and_then(AttachedPane::window) else {
        return;
    };
    activate_keyboard(state, pane, window);
    if state.keyboard_state.held.contains(&KEY_V) {
        return;
    }
    if let Some(keyboard) = state.keyboard.take() {
        let previous = keyboard.modifier_state();
        set_modifiers(&keyboard, state, snapshot(modifiers::CONTROL));
        forward(&keyboard, state, KEY_V, KeyState::Pressed);
        forward(&keyboard, state, KEY_V, KeyState::Released);
        set_modifiers(&keyboard, state, previous);
        state.keyboard = Some(keyboard);
    }
}
