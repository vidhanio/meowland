//! Terminal-pane keyboard and pointer events mapped into Wayland input.

use std::collections::{HashMap, VecDeque, hash_map::Entry};

use meowland_protocol::{
    Button, Input, KEY_V, MAX_HELD_KEYS, Modifiers, Scroll, Transition, valid_key_identity,
};
use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState as WaylandButtonState, InputTime, KeyState, Keycode,
    },
    input::{
        keyboard::{KeyboardHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::{Resource, backend::ClientId},
    utils::{Point, SERIAL_COUNTER, Serial},
};

use super::{AttachedPane, Compositor, PaneId, WindowId, WlSurface, keymap, text};

/// Which pane the Wayland keyboard follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Owner {
    #[default]
    Nobody,
    /// The pane focused the keyboard without reporting its held keys.
    Activated(PaneId),
    /// The held keys match the pane's latest snapshot.
    Synced(PaneId),
}

impl Owner {
    const fn pane(self) -> Option<PaneId> {
        match self {
            Self::Nobody => None,
            Self::Activated(pane) | Self::Synced(pane) => Some(pane),
        }
    }
}

/// Whether the focused client saw a held key's press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    Forwarded,
    /// Pressed while a grab owned the keyboard; its release is not forwarded.
    Suppressed,
}

#[derive(Clone, Copy, Debug)]
struct Held {
    code: u16,
    delivery: Delivery,
}

#[derive(Default)]
pub(crate) struct KeyboardState {
    owner: Owner,
    held: HashMap<u32, Held>,
    serials: VecDeque<(Serial, ClientId)>,
}

impl KeyboardState {
    pub(crate) fn owned_by(&self, pane: PaneId) -> bool {
        self.owner.pane() == Some(pane)
    }

    /// Stop accepting serials sent before a keyboard focus change.
    pub(crate) fn forget_serials(&mut self) {
        self.serials.clear();
    }
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
/// Wheel units the terminal reports for one detent, which matches Wayland's
/// value120 scale, so terminal units are forwarded as value120 unchanged.
const SCROLL_UNITS_PER_DETENT: i32 = 120;
/// Wayland axis distance emitted for one wheel detent.
const AXIS_UNITS_PER_DETENT: f64 = 7.5;

pub(crate) fn pane_input(state: &mut Compositor, pane: PaneId, event: &Input) {
    if !valid_input(event) {
        return;
    }
    state.refresh_popup_grab();
    if matches!(event, Input::ResetKeyboard) {
        if state.input.keyboard_state.owned_by(pane) {
            reset_keyboard(state);
        }
        return;
    }
    if matches!(
        event,
        Input::Key {
            state: Transition::Released,
            ..
        }
    ) && !state.input.keyboard_state.owned_by(pane)
    {
        return;
    }
    let Some(window) = state.attachments.get(pane).and_then(AttachedPane::window) else {
        return;
    };
    if !state.windows.catalog.drawable(window) {
        return;
    }
    state.input.last_input_pane = Some(pane);
    if state.attachments.decide(window, pane) {
        state.apply_window_state(window);
    }
    match *event {
        Input::Key {
            key,
            state: transition,
            modifiers,
            ref text,
            ref held,
        } => {
            keymap::refresh(state);
            if state.input.keyboard_state.owner == Owner::Synced(pane) {
                activate_keyboard(state, pane, window);
            } else {
                enter_keyboard(state, pane, window, held, modifiers);
            }
            key_transition(
                state,
                key,
                match transition {
                    Transition::Pressed => KeyState::Pressed,
                    Transition::Released => KeyState::Released,
                },
                modifiers,
                text.as_deref(),
            );
        }
        Input::KeyboardEnter {
            ref keys,
            modifiers,
        } => {
            keymap::refresh(state);
            enter_keyboard(state, pane, window, keys, modifiers);
        }
        Input::ResetKeyboard => unreachable!("keyboard resets return before window routing"),
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
            let grabbed = state.input.pointer.is_grabbed();
            if !grabbed && button.is_some_and(|(_, transition)| transition == Transition::Pressed) {
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
        Input::Key { key, held, .. } => valid_key_identity(*key) && valid_keys(held),
        Input::KeyboardEnter { keys, .. } => valid_keys(keys),
        Input::Pointer { x, y, .. } => x.is_finite() && y.is_finite(),
        Input::Text(_) | Input::ResetKeyboard => true,
    }
}

fn valid_keys(keys: &[u32]) -> bool {
    keys.len() <= MAX_HELD_KEYS && keys.iter().all(|key| valid_key_identity(*key))
}

/// Route one key transition through the active layout or the text fallback.
fn key_transition(
    state: &mut Compositor,
    key: u32,
    transition: KeyState,
    modifiers: Modifiers,
    text: Option<&str>,
) {
    let code = state.input.keymap.lookup(key);
    if transition == KeyState::Pressed {
        if let Some(code) = code {
            if let Some(keyboard) = state.input.keyboard.clone() {
                inject_key(&keyboard, state, key, code, transition, modifiers);
            }
        } else if let Some(text) = key_text(key, text, modifiers) {
            text::commit(state, &text);
        } else if modifiers.intersects(Modifiers::SHORTCUT) {
            tracing::debug!(
                key,
                ?modifiers,
                "dropping modified key the active layout cannot type"
            );
        }
    } else {
        let code = code.or_else(|| {
            state
                .input
                .keyboard_state
                .held
                .get(&key)
                .map(|held| held.code)
        });
        if let Some(code) = code
            && let Some(keyboard) = state.input.keyboard.clone()
        {
            inject_key(&keyboard, state, key, code, transition, modifiers);
        }
    }
}

/// Text an unmapped key commits when no shortcut owns it.
fn key_text(key: u32, text: Option<&str>, bits: Modifiers) -> Option<String> {
    if bits.intersects(Modifiers::SHORTCUT) {
        return None;
    }
    text.map(str::to_owned).or_else(|| keymap::text(key))
}

fn pointer_input(
    state: &mut Compositor,
    window: WindowId,
    position: (f64, f64),
    button: Option<(Button, Transition)>,
    scroll: Scroll,
) {
    let (x, y) = position;
    let root = state
        .windows
        .by_id
        .get(&window)
        .map(|window| window.surface.wl_surface());
    let hit = root.and_then(|root| {
        state
            .renderer
            .hit_test(root, &state.windows.surface_cache.snapshots, x, y)
    });
    let location = Point::from((x, y));
    state.dispatch_pointer(|state, pointer| {
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
                        Transition::Pressed => WaylandButtonState::Pressed,
                        Transition::Released => WaylandButtonState::Released,
                    },
                    serial,
                    time,
                },
            );
        }
        if let Some(frame) = wheel_frame(scroll, time) {
            pointer.frame(state);
            pointer.axis(state, frame);
        }
        pointer.frame(state);
    });
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

/// Wayland axis frame for a terminal wheel movement, if it moved at all.
fn wheel_frame(scroll: Scroll, time: InputTime) -> Option<AxisFrame> {
    let vertical = -i32::from(scroll.vertical);
    let horizontal = i32::from(scroll.horizontal);
    if vertical == 0 && horizontal == 0 {
        return None;
    }
    let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
    for (axis, units) in [(Axis::Vertical, vertical), (Axis::Horizontal, horizontal)] {
        if units != 0 {
            frame = frame.value(axis, axis_units(units)).v120(axis, units);
        }
    }
    Some(frame)
}

/// Convert a wheel delta in terminal units to Wayland axis distance.
fn axis_units(units: i32) -> f64 {
    f64::from(units) * AXIS_UNITS_PER_DETENT / f64::from(SCROLL_UNITS_PER_DETENT)
}

fn enter_keyboard(
    state: &mut Compositor,
    pane: PaneId,
    window: WindowId,
    keys: &[u32],
    bits: Modifiers,
) {
    reset_keyboard(state);
    if !keyboard_grabbed(state) {
        state.focus_window(window);
    }
    if let Some(keyboard) = state.input.keyboard.clone() {
        state.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
        set_modifiers(&keyboard, state, snapshot(bits));
        for key in keys {
            let Some(code) = state.input.keymap.lookup(*key) else {
                continue;
            };
            if state.input.keyboard_state.held.contains_key(key) {
                continue;
            }
            let delivery = if keyboard_grabbed(state) {
                Delivery::Suppressed
            } else {
                forward(&keyboard, state, code, KeyState::Pressed);
                Delivery::Forwarded
            };
            state
                .input
                .keyboard_state
                .held
                .insert(*key, Held { code, delivery });
        }
        let surface = state.windows.by_id[&window].surface.wl_surface().clone();
        state.set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
    }
    state.input.keyboard_state.owner = Owner::Synced(pane);
}

fn activate_keyboard(state: &mut Compositor, pane: PaneId, window: WindowId) {
    let changed = !state.input.keyboard_state.owned_by(pane);
    if changed {
        reset_keyboard(state);
    }
    if !keyboard_grabbed(state) {
        state.focus_window(window);
        if changed && state.input.keyboard.is_some() {
            let surface = state.windows.by_id[&window].surface.wl_surface().clone();
            state.set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
        }
    }
    if changed {
        state.input.keyboard_state.owner = Owner::Activated(pane);
    }
}

fn keyboard_grabbed(state: &Compositor) -> bool {
    state
        .input
        .keyboard
        .as_ref()
        .is_some_and(KeyboardHandle::is_grabbed)
}

fn event_serial(state: &mut Compositor, focus: Option<WlSurface>) -> Serial {
    let serial = SERIAL_COUNTER.next_serial();
    if let Some(client) = focus.and_then(|surface| surface.client()) {
        state
            .input
            .keyboard_state
            .serials
            .push_back((serial, client.id()));
        if state.input.keyboard_state.serials.len() > SERIAL_HISTORY {
            state.input.keyboard_state.serials.pop_front();
        }
    }
    serial
}

pub(crate) fn valid_serial(state: &Compositor, serial: Serial, surface: &WlSurface) -> bool {
    surface.client().is_some_and(|client| {
        let keyboard = state
            .input
            .keyboard
            .as_ref()
            .and_then(KeyboardHandle::current_focus);
        let pointer = state.input.pointer.current_focus();
        let root = state.window_for_surface(surface);
        let focused = root.is_some()
            && keyboard
                .into_iter()
                .chain(pointer)
                .any(|focus| state.window_for_surface(&focus) == root);
        focused
            && state
                .input
                .keyboard_state
                .serials
                .iter()
                .any(|(recent, recipient)| *recent == serial && *recipient == client.id())
    })
}

/// A pane cannot leave keys or snapshot-only modifiers down when it loses
/// focus, disconnects, or starts showing a different window.
pub(crate) fn reset_keyboard(state: &mut Compositor) {
    state.input.keyboard_state.owner = Owner::Nobody;
    let held = std::mem::take(&mut state.input.keyboard_state.held);
    if let Some(keyboard) = state.input.keyboard.clone() {
        set_modifiers(&keyboard, state, ModifiersState::default());
        for held in held.into_values() {
            if held.delivery == Delivery::Forwarded {
                forward(&keyboard, state, held.code, KeyState::Released);
            }
        }
        state.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
    }
}

fn snapshot(bits: Modifiers) -> ModifiersState {
    ModifiersState {
        shift: bits.contains(Modifiers::SHIFT),
        ctrl: bits.contains(Modifiers::CONTROL),
        alt: bits.contains(Modifiers::ALT),
        logo: bits.contains(Modifiers::SUPER),
        caps_lock: bits.contains(Modifiers::CAPS_LOCK),
        num_lock: bits.contains(Modifiers::NUM_LOCK),
        ..ModifiersState::default()
    }
}

fn set_modifiers(
    keyboard: &KeyboardHandle<Compositor>,
    state: &mut Compositor,
    modifiers: ModifiersState,
) {
    if keyboard.set_modifier_state(modifiers) != 0 {
        keyboard.advertise_modifier_state(state);
    }
}

/// The terminal is the modifier authority, like a parent Wayland seat. Do not
/// re-derive its state by feeding logical key identities through XKB again.
fn inject_key(
    keyboard: &KeyboardHandle<Compositor>,
    state: &mut Compositor,
    key: u32,
    code: u16,
    transition: KeyState,
    bits: Modifiers,
) {
    set_modifiers(keyboard, state, snapshot(bits));
    let held = &mut state.input.keyboard_state.held;
    let (changed, code) = if transition == KeyState::Pressed {
        let entry = held.entry(key);
        let fresh = matches!(entry, Entry::Vacant(_));
        entry.or_insert(Held {
            code,
            delivery: Delivery::Forwarded,
        });
        (fresh, code)
    } else if let Some(Held { code, delivery }) = held.remove(&key) {
        (delivery == Delivery::Forwarded, code)
    } else {
        (false, code)
    };
    if changed {
        forward(keyboard, state, code, transition);
    }
}

fn forward(
    keyboard: &KeyboardHandle<Compositor>,
    state: &mut Compositor,
    code: u16,
    transition: KeyState,
) {
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
pub(crate) fn paste_action(state: &mut Compositor, pane: PaneId) {
    let Some(window) = state.attachments.get(pane).and_then(AttachedPane::window) else {
        return;
    };
    activate_keyboard(state, pane, window);
    if state
        .input
        .keyboard_state
        .held
        .get(&KEY_V)
        .is_some_and(|held| held.delivery == Delivery::Forwarded)
    {
        return;
    }
    let Some(code) = state.input.keymap.lookup(KEY_V) else {
        return;
    };
    if let Some(keyboard) = state.input.keyboard.clone() {
        let previous = keyboard.modifier_state();
        set_modifiers(&keyboard, state, snapshot(Modifiers::CONTROL));
        forward(&keyboard, state, code, KeyState::Pressed);
        forward(&keyboard, state, code, KeyState::Released);
        set_modifiers(&keyboard, state, previous);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use smithay::input::keyboard::{GrabStartData, KeyboardGrab, KeyboardInnerHandle};

    use super::*;

    struct RecordingGrab {
        start: GrabStartData<Compositor>,
        events: Arc<Mutex<Vec<KeyState>>>,
    }

    impl KeyboardGrab<Compositor> for RecordingGrab {
        fn input(
            &mut self,
            _: &mut Compositor,
            _: &mut KeyboardInnerHandle<'_, Compositor>,
            _: Keycode,
            state: KeyState,
            _: Option<ModifiersState>,
            _: Serial,
            _: InputTime,
        ) {
            self.events.lock().expect("grab events lock").push(state);
        }

        fn set_focus(
            &mut self,
            _: &mut Compositor,
            _: &mut KeyboardInnerHandle<'_, Compositor>,
            _: Option<WlSurface>,
            _: Serial,
        ) {
        }

        fn start_data(&self) -> &GrabStartData<Compositor> {
            &self.start
        }

        fn unset(&mut self, _: &mut Compositor) {}
    }

    #[test]
    fn grabbed_snapshot_keys_do_not_emit_unmatched_releases() {
        let mut fixture = crate::fixture::Fixture::new();
        let toplevel = fixture.toplevel("app");
        let window = fixture.window(&toplevel);
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let keyboard = fixture.state.input.keyboard.clone().expect("keyboard");
        let events = Arc::new(Mutex::new(Vec::new()));
        keyboard.set_grab(
            &mut fixture.state,
            RecordingGrab {
                start: GrabStartData { focus: None },
                events: Arc::clone(&events),
            },
            SERIAL_COUNTER.next_serial(),
        );
        enter_keyboard(
            &mut fixture.state,
            PaneId::from(1),
            window,
            &[u32::from('a'), u32::from('b')],
            Modifiers::empty(),
        );
        key_transition(
            &mut fixture.state,
            u32::from('a'),
            KeyState::Released,
            Modifiers::empty(),
            None,
        );
        reset_keyboard(&mut fixture.state);
        assert!(events.lock().expect("grab events lock").is_empty());
    }

    #[test]
    fn serial_from_another_window_of_the_same_client_cannot_grab() {
        let mut fixture = crate::fixture::Fixture::new();
        let first = fixture.toplevel("first");
        fixture.present(&first.surface, 4, 4, [0, 0, 0, 255]);
        let first_id = fixture.window(&first);
        let first_surface = fixture.state.windows.by_id[&first_id]
            .surface
            .wl_surface()
            .clone();
        let second = fixture.toplevel("second");
        fixture.present(&second.surface, 4, 4, [0, 0, 0, 255]);
        let second_id = fixture.window(&second);
        let second_surface = fixture.state.windows.by_id[&second_id]
            .surface
            .wl_surface()
            .clone();
        let serial = event_serial(&mut fixture.state, Some(second_surface.clone()));
        assert!(valid_serial(&fixture.state, serial, &second_surface));
        assert!(!valid_serial(&fixture.state, serial, &first_surface));
    }

    fn frame(vertical: i16, horizontal: i16) -> Option<AxisFrame> {
        wheel_frame(
            Scroll {
                vertical,
                horizontal,
            },
            InputTime::now(),
        )
    }

    #[test]
    fn wheel_frames_carry_one_value120_unit_per_terminal_unit() {
        for (vertical, horizontal, v120) in [
            (120, 0, (0, -120)),
            (-120, 0, (0, 120)),
            (1, 0, (0, -1)),
            (-1, 0, (0, 1)),
            (0, 120, (120, 0)),
            (0, -120, (-120, 0)),
            (0, 1, (1, 0)),
            (0, -1, (-1, 0)),
            (240, -1, (-1, -240)),
        ] {
            let frame = frame(vertical, horizontal).expect("wheel moved");
            assert_eq!(frame.v120, Some(v120), "{vertical} {horizontal}");
            assert_eq!(frame.source, Some(AxisSource::Wheel));
        }
    }

    #[test]
    fn still_wheel_sends_no_axis_frame() {
        assert!(frame(0, 0).is_none());
    }
}
