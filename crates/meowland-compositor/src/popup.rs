//! Popup-grab lifetime and focus restoration.

use smithay::{
    backend::input::InputTime, reexports::wayland_server::Resource, utils::SERIAL_COUNTER,
};

use super::State;

impl State {
    pub fn refresh_popup_grab(&mut self) {
        self.popups.cleanup();
        if self
            .popup_grab
            .as_ref()
            .is_none_or(|grab| !grab.has_ended())
        {
            return;
        }
        let grab = self.popup_grab.take().unwrap();
        if let Some(keyboard) = self.keyboard.take() {
            if keyboard.has_grab(grab.serial()) {
                keyboard.unset_grab(self);
                let focus = grab.current_grab().filter(Resource::is_alive);
                keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
            }
            self.keyboard = Some(keyboard);
        }
        if let Some(pointer) = self.pointer.take() {
            if pointer.has_grab(grab.serial()) {
                pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
            }
            self.pointer = Some(pointer);
        }
    }
}
