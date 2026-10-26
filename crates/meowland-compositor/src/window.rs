//! Coordinated window map, unmap and destruction transitions.

use smithay::desktop::PopupUngrabStrategy;

use super::{Policy, ServerToPane, State, WindowId, WlSurface, input};

impl State {
    pub(crate) fn refresh_window_mapping(&mut self, surface: &WlSurface) {
        let Some(id) = self.ids.get(surface).copied() else {
            return;
        };
        if self.surface_cache.snapshots.contains_key(surface) {
            self.map_window(id);
        } else {
            self.unmap_window(id);
        }
    }

    fn map_window(&mut self, id: WindowId) {
        if !self.catalog.map(id) {
            return;
        }
        let info = self.window_info(id);
        self.toplevels.announce(id, &info.title, &info.app_id);
        self.touch(id);
        self.follow_panes(id);
        self.resolve_pending_panes();
        self.apply_window_state(id);
        self.enter_output(id);
        self.focus_window(id);
    }

    pub(crate) fn unmap_window(&mut self, id: WindowId) {
        let focused = self.catalog.focused() == Some(id);
        if !self.catalog.unmap(id) {
            return;
        }
        self.toplevels.remove(id);
        self.leave_output(id);
        self.clear_window_pointer(id);
        self.touch(id);
        if focused {
            if let Some(grab) = &mut self.popup_grab {
                grab.ungrab(PopupUngrabStrategy::All);
            }
            self.refresh_popup_grab();
            input::reset_keyboard(self);
        }
        for pane in self.panes_showing(id) {
            if self.attachments.panes[&pane].policy != Policy::FollowNewest {
                continue;
            }
            if let Some(next) = self.catalog.newest() {
                self.bind_pane_window(pane, next);
            } else {
                self.attachments.panes.get_mut(&pane).unwrap().clear();
                self.emit(pane, ServerToPane::Title(String::new()));
                self.send_pane_cursor(pane);
            }
        }
        if focused {
            let next = self
                .attachments
                .first_window()
                .filter(|&id| self.catalog.drawable(id))
                .or_else(|| self.catalog.newest());
            if let Some(next) = next {
                self.focus_window(next);
            }
        }
    }

    pub(crate) fn remove_window(&mut self, id: WindowId) {
        self.unmap_window(id);
        self.cancel_window_activations(id);
        for pane in self.panes_showing(id) {
            self.remove_pane(pane);
            self.emit(pane, ServerToPane::Release("window closed".into()));
        }
        self.surface_cache.remove_window(id);
        if let Some(window) = self.windows.remove(&id) {
            self.ids.remove(window.surface.wl_surface());
        }
        self.toplevels.remove(id);
        self.attachments.forget_window(id);
        self.catalog.remove(id);
        self.popups.cleanup();
    }
}
