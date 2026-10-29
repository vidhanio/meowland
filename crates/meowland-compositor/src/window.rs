//! Coordinated window map, unmap and destruction transitions.

use std::time::Duration;

use smithay::{
    desktop::{
        PopupManager, PopupUngrabStrategy,
        utils::{send_frames_surface_tree, with_surfaces_surface_tree},
    },
    output::Output,
};

use super::{Policy, ServerToPane, State, Window, WindowId, WlSurface, clear_owner, input};

impl Window {
    fn with_surface_trees(&self, mut visit: impl FnMut(&WlSurface)) {
        let root = self.surface.wl_surface();
        visit(root);
        for (popup, _) in PopupManager::popups_for_surface(root) {
            visit(popup.wl_surface());
        }
    }

    /// Visit the window's surface trees without building a render stack.
    pub(crate) fn with_surfaces(&self, mut visit: impl FnMut(&WlSurface)) {
        self.with_surface_trees(|root| {
            with_surfaces_surface_tree(root, |surface, _| visit(surface));
        });
    }

    /// Deliver callbacks at the compositor's already-paced frame deadline.
    pub(crate) fn send_frame(&self, output: &Output, time: Duration) {
        self.with_surface_trees(|root| {
            send_frames_surface_tree(root, output, time, Some(Duration::ZERO), |_, _| None);
        });
    }
}

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
        self.popups_dirty = true;
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
            if self
                .attachments
                .get(pane)
                .is_none_or(|entry| entry.policy != Policy::FollowNewest)
            {
                continue;
            }
            if let Some(next) = self.catalog.newest() {
                self.bind_pane_window(pane, next);
            } else {
                self.attachments.clear(pane);
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
        let surfaces: Vec<_> = self
            .surface_cache
            .snapshots
            .keys()
            .chain(self.surface_cache.spares.keys())
            .filter(|surface| self.window_for_surface(surface) == Some(id))
            .cloned()
            .collect();
        for surface in surfaces {
            self.surface_cache.forget(&surface);
        }
        if let Some(window) = self.windows.remove(&id) {
            window.with_surfaces(clear_owner);
            self.ids.remove(window.surface.wl_surface());
        }
        self.refresh_callback_deadline();
        self.toplevels.remove(id);
        self.attachments.forget_window(id);
        self.catalog.remove(id);
        self.popups.cleanup();
        self.popups_dirty = false;
    }
}
