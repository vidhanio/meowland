//! What the terminal reports, as the client sees it: keys, pointer, and paste.

use evdev::KeyCode;
use smithay::{
    backend::input::{Axis, ButtonState, KeyState},
    input::{
        keyboard::{FilterResult, Keycode},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use super::Compositor;
use crate::{
    keys,
    protocol::{
        PaneId,
        pane::{Input, Key, Pointer},
    },
};

pub const BINDING_MODIFIER: crossterm::event::KeyModifiers = crossterm::event::KeyModifiers::ALT;

impl Compositor {
    /// What the user did in this pane, as the pane reported it.
    pub fn input(&mut self, pane: PaneId, input: Input) {
        match input {
            Input::Key(key) => self.key(pane, key),
            Input::Pointer(pointer) => self.pointer(pane, pointer),
            Input::Paste(text) => {
                self.interact(pane);
                self.paste(&text);
            }
            Input::Focus(true) => self.interact(pane),
            Input::Focus(false) => {}
        }
    }

    /// Handle a key the terminal reported.
    ///
    /// Terminals do not have to report key releases. So a press is treated as a
    /// whole keystroke: press the key, release it, and let the terminal's
    /// auto-repeat produce the repeats. A held key cannot otherwise be told
    /// from one that was never released, and the client would repeat it
    /// forever.
    ///
    /// The terminal reduces the key to a code and a shift first
    /// (`crate::display`), because it has the key codes and the keymap.
    /// Typing in a pane gives its window the keyboard, and the bindings act
    /// on that window.
    pub fn key(&mut self, pane: PaneId, key: Key) {
        use crate::protocol::pane::KeyKind;

        let modifiers = crossterm::event::KeyModifiers::from_bits_truncate(key.modifiers);
        self.sync_modifiers(modifiers);
        let stroke = keys::KeyStroke {
            code: key.code,
            shift: key.shift,
        };
        tracing::debug!(?key, code = stroke.code.code(), "key");

        // Modifier keys are *state* for everything typed while they are held,
        // so they follow the terminal's flags, not a keystroke.
        if key.modifier {
            match key.kind {
                KeyKind::Press => self.press_modifier(stroke.code),
                KeyKind::Repeat => {}
                KeyKind::Release => self.release_modifier(stroke.code),
            }
            return;
        }

        match key.kind {
            KeyKind::Press | KeyKind::Repeat => {
                if !self.binding(pane, modifiers, stroke.code) {
                    self.interact(pane);
                    self.type_stroke(stroke);
                }
            }
            // The press already released this key.
            KeyKind::Release => {}
        }
    }

    /// Press a key and release it, holding shift while its symbol needs it.
    fn type_stroke(&mut self, stroke: keys::KeyStroke) {
        let synthesized_shift = stroke.shift && !self.is_pressed(KeyCode::KEY_LEFTSHIFT);
        if synthesized_shift {
            self.press_modifier(KeyCode::KEY_LEFTSHIFT);
        }
        self.forward_key(stroke.code, KeyState::Pressed);
        self.forward_key(stroke.code, KeyState::Released);
        if synthesized_shift {
            self.release_modifier(KeyCode::KEY_LEFTSHIFT);
        }
    }

    /// Handle compositor bindings before a key reaches a client.
    ///
    /// A binding acts on the pane the key was typed in, so it closes the window
    /// that pane shows or detaches the pane.
    fn binding(
        &mut self,
        pane: PaneId,
        modifiers: crossterm::event::KeyModifiers,
        code: KeyCode,
    ) -> bool {
        use crossterm::event::KeyModifiers as M;
        if !modifiers.contains(BINDING_MODIFIER)
            || modifiers.contains(M::CONTROL)
            || modifiers.contains(M::SUPER)
        {
            return false;
        }
        // Linux input event codes, as the terminal reports them
        // (`keys::for_char`): `KEY_Q` and `KEY_W`.
        match code {
            KeyCode::KEY_Q => {
                // Close the window this pane shows, if it has one. Closing a
                // client ends its pane; a pane with nothing to
                // show releases its terminal instead.
                if self.pane_window(pane).is_some() {
                    self.close_window(pane);
                } else if let Some(index) = self.view(pane) {
                    self.leave(index, None);
                }
            }
            KeyCode::KEY_W => {
                if let Some(index) = self.view(pane) {
                    self.leave(index, None);
                }
            }
            _ => return false,
        }
        true
    }

    /// Type text into the focused client, one keystroke per character.
    ///
    /// Text arrives as characters and clients take key presses, so each
    /// character is matched to the stroke that produces it.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            let stroke = match c {
                '\n' | '\r' => keys::for_key(crossterm::event::KeyCode::Enter),
                _ => keys::for_char(c),
            };
            if let Some(stroke) = stroke {
                self.type_stroke(stroke);
            } else {
                tracing::debug!(?c, "character has no key code in the advertised keymap");
            }
        }
    }

    fn sync_modifiers(&mut self, modifiers: crossterm::event::KeyModifiers) {
        use crossterm::event::KeyModifiers as M;
        for (flag, code) in [
            (M::SHIFT, KeyCode::KEY_LEFTSHIFT),
            (M::CONTROL, KeyCode::KEY_LEFTCTRL),
            (M::ALT, KeyCode::KEY_LEFTALT),
            (M::SUPER, KeyCode::KEY_LEFTMETA),
        ] {
            if modifiers.contains(flag) {
                self.press_modifier(code);
            } else {
                self.release_modifier(code);
            }
        }
    }

    fn press_modifier(&mut self, code: KeyCode) {
        if !self.is_pressed(code) {
            self.forward_key(code, KeyState::Pressed);
        }
    }

    fn release_modifier(&mut self, code: KeyCode) {
        if self.is_pressed(code) {
            self.forward_key(code, KeyState::Released);
        }
    }

    fn is_pressed(&self, code: KeyCode) -> bool {
        self.pressed.contains(&code)
    }

    /// Hand a key to the focused client.
    ///
    /// The rest of this module counts keys as the terminal and `KEY_*` do
    /// (evdev); the seat counts as XKB does, eight codes further along, so
    /// the conversion happens here and nowhere else. Confusing the two is
    /// not a loud failure: it types the neighbouring key.
    fn forward_key(&mut self, code: KeyCode, state: KeyState) {
        tracing::debug!(code = code.code(), ?state, "forwarding key");
        if state == KeyState::Pressed {
            if !self.pressed.contains(&code) {
                self.pressed.push(code);
            }
        } else {
            self.pressed.retain(|pressed| *pressed != code);
        }
        let keyboard = self.keyboard.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        keyboard.input(
            self,
            Keycode::from(u32::from(code.code()) + keys::XKB_OFFSET),
            state,
            serial,
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }

    /// Handle a mouse event in a pane, in the cells the terminal reports.
    pub fn pointer(&mut self, pane: PaneId, pointer: Pointer) {
        let Some(index) = self.view(pane) else {
            return;
        };
        match pointer {
            Pointer::Motion { column, row } => {
                let position = self.cell_position(index, column, row);
                self.pointer_motion(index, position);
            }
            Pointer::Button {
                column,
                row,
                button,
                pressed,
            } => {
                // A click goes to the window the pane shows, so give it the
                // keyboard.
                self.interact(pane);
                let position = self.cell_position(index, column, row);
                self.pointer_motion(index, position);
                self.pointer_button(button, pressed);
            }
            Pointer::ScrollUp | Pointer::ScrollLeft => {
                self.interact(pane);
                self.pointer_axis(index, -15.0);
            }
            Pointer::ScrollDown | Pointer::ScrollRight => {
                self.interact(pane);
                self.pointer_axis(index, 15.0);
            }
        }
    }

    /// Where an event at a terminal cell happened, in the pixels that pane
    /// draws in.
    fn cell_position(&self, pane: usize, column: u16, row: u16) -> Point<f64, Logical> {
        let capabilities = &self.views[pane].capabilities;
        if capabilities.pixel_mouse {
            return (f64::from(column), f64::from(row)).into();
        }
        let (cell_width, cell_height) = capabilities.cell;
        (
            f64::mul_add(
                f64::from(column),
                f64::from(cell_width),
                f64::from(cell_width) / 2.0,
            ),
            f64::mul_add(
                f64::from(row),
                f64::from(cell_height),
                f64::from(cell_height) / 2.0,
            ),
        )
            .into()
    }

    fn pointer_motion(&mut self, pane: usize, position: Point<f64, Logical>) {
        // The seat takes the pointer position in *output* coordinates and the
        // origin of the focused surface, and subtracts the two to get
        // the client's position.
        let event = MotionEvent {
            location: position,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
        };
        let pointer = self.pointer.clone();
        if let Some((surface, origin)) = self.surface_at(pane, position) {
            pointer.motion(self, Some((surface, origin.to_f64())), &event);
        } else {
            pointer.motion(self, None, &event);
        }
        pointer.frame(self);
    }

    fn pointer_button(&mut self, button: KeyCode, pressed: bool) {
        let pointer = self.pointer.clone();
        let event = ButtonEvent {
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
            button: u32::from(button.code()),
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        };
        pointer.button(self, &event);
        pointer.frame(self);
    }

    fn pointer_axis(&mut self, pane: usize, vertical: f64) {
        let pointer = self.pointer.clone();
        // The wheel has no position of its own: it acts where the pointer is,
        // which the seat's pointer handle knows.
        let position = pointer.current_location();
        if let Some((surface, origin)) = self.surface_at(pane, position) {
            let event = MotionEvent {
                location: position,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.time(),
            };
            pointer.motion(self, Some((surface, origin.to_f64())), &event);
        }
        let frame = AxisFrame::new(self.time()).value(Axis::Vertical, vertical);
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::pane::{Capabilities, Show},
        wayland::message::Event,
    };

    fn capabilities(pixels: (u32, u32)) -> Capabilities {
        Capabilities {
            cell: (10, 20),
            cells: (pixels.0 / 10, pixels.1 / 20),
            pixels,
            terminal: None,
            graphics: true,
            keyboard: true,
            pixel_mouse: true,
            shared_memory: false,
            patches: true,
        }
    }

    #[test]
    fn alt_w_releases_the_pane() {
        let display =
            smithay::reexports::wayland_server::Display::<Compositor>::new().expect("display");
        let (events, released) = calloop::channel::channel();
        let mut state = Compositor::new(&display.handle(), &[], events).expect("compositor");
        let pane = PaneId::new(1);
        state.attach_view(pane, Show::Focused, &capabilities((800, 600)));

        assert!(state.binding(pane, BINDING_MODIFIER, KeyCode::KEY_W));
        let told = std::iter::from_fn(|| released.try_recv().ok())
            .any(|event| matches!(event, Event::PaneDone { pane: done, .. } if done == pane));
        assert!(
            told,
            "the server is told that the pane has nothing left to show"
        );
    }
}
