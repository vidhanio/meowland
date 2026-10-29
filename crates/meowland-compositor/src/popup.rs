//! Popup-grab lifetime and focus restoration.

use smithay::{
    backend::input::InputTime, reexports::wayland_server::Resource, utils::SERIAL_COUNTER,
};

use super::Compositor;

impl Compositor {
    pub(crate) fn refresh_popup_grab(&mut self) {
        if self.input.popup_grab.is_none() && !self.windows.popups_dirty {
            return;
        }
        self.windows.popups.cleanup();
        self.windows.popups_dirty = false;
        let Some(grab) = self.input.popup_grab.take_if(|grab| grab.has_ended()) else {
            return;
        };
        if let Some(keyboard) = self.input.keyboard.clone()
            && keyboard.has_grab(grab.serial())
        {
            keyboard.unset_grab(self);
            let focus = grab.current_grab().filter(Resource::is_alive);
            keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
        }
        if let Some(pointer) = self.input.pointer.take() {
            if pointer.has_grab(grab.serial()) {
                pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
            }
            self.input.pointer = Some(pointer);
        }
    }
}
