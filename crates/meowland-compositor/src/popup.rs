//! Popup-grab lifetime and focus restoration.

use smithay::{
    backend::input::InputTime, reexports::wayland_server::Resource, utils::SERIAL_COUNTER,
};

use super::State;

impl State {
    pub(crate) fn refresh_popup_grab(&mut self) {
        if self.popup_grab.is_none() && !self.popups_dirty {
            return;
        }
        self.popups.cleanup();
        self.popups_dirty = false;
        let Some(grab) = self.popup_grab.take_if(|grab| grab.has_ended()) else {
            return;
        };
        if let Some(keyboard) = self.keyboard.clone()
            && keyboard.has_grab(grab.serial())
        {
            keyboard.unset_grab(self);
            let focus = grab.current_grab().filter(Resource::is_alive);
            keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
        }
        if let Some(pointer) = self.pointer.take() {
            if pointer.has_grab(grab.serial()) {
                pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
            }
            self.pointer = Some(pointer);
        }
    }
}
