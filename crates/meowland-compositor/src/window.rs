//! Coordinated window map, unmap and destruction transitions.

use std::time::Duration;

use smithay::{
    desktop::{
        PopupManager, PopupUngrabStrategy,
        utils::{send_frames_surface_tree, with_surfaces_surface_tree},
    },
    output::Output,
    wayland::compositor::{BufferAssignment, SurfaceData},
};

use super::{Compositor, Remap, ServerToPane, Window, WindowId, WlSurface, input};

impl Window {
    fn with_surface_trees(&self, mut visit: impl FnMut(&WlSurface)) {
        let root = self.surface.wl_surface();
        visit(root);
        for (popup, _) in PopupManager::popups_for_surface(root) {
            visit(popup.wl_surface());
        }
    }

    /// Visit the window's surface trees without building a render stack.
    ///
    /// The visitor runs with the surface's state locked and receives that
    /// state; calling `with_states` on the surface inside it deadlocks.
    pub(crate) fn with_surfaces(&self, mut visit: impl FnMut(&WlSurface, &SurfaceData)) {
        self.with_surface_trees(|root| {
            with_surfaces_surface_tree(root, &mut visit);
        });
    }

    /// Deliver callbacks at the compositor's already-paced frame deadline.
    pub(crate) fn send_frame(&self, output: &Output, time: Duration) {
        self.with_surface_trees(|root| {
            send_frames_surface_tree(root, output, time, Some(Duration::ZERO), |_, _| None);
        });
    }
}

impl Compositor {
    pub(crate) fn update_remap_phase(
        &mut self,
        surface: &WlSurface,
        window: Option<WindowId>,
        assignment: Option<&BufferAssignment>,
    ) {
        if let Some(entry) = window.and_then(|id| self.windows.by_id.get_mut(&id))
            && entry.surface.wl_surface() == surface
        {
            entry.remap = match (entry.remap, assignment) {
                (Remap::AwaitingInitialCommit, None | Some(BufferAssignment::Removed)) => {
                    entry.surface.send_configure();
                    Remap::Idle
                }
                (Remap::Idle, Some(BufferAssignment::Removed)) => Remap::AwaitingInitialCommit,
                (_, Some(BufferAssignment::NewBuffer(_))) | (Remap::Idle, None) => Remap::Idle,
            };
        }
    }

    pub(crate) fn refresh_window_mapping(&mut self, surface: &WlSurface) {
        let Some(id) = self.windows.id_by_surface.get(surface).copied() else {
            return;
        };
        if self.windows.surface_cache.snapshots().contains_key(surface) {
            self.map_window(id);
        } else {
            self.unmap_window(id);
        }
    }

    fn map_window(&mut self, id: WindowId) {
        if !self.windows.catalog.map(id) {
            return;
        }
        self.touch(id);
        self.resolve_pending_panes();
        self.apply_window_state(id);
        self.enter_output(id);
        self.focus_window(id);
    }

    pub(crate) fn unmap_window(&mut self, id: WindowId) {
        let focused = self.windows.catalog.focused() == Some(id);
        if !self.windows.catalog.unmap(id) {
            return;
        }
        self.windows.popups_dirty = true;
        self.leave_output(id);
        self.clear_window_pointer(id);
        self.touch(id);
        if focused {
            if let Some(grab) = &mut self.input.popup_grab {
                grab.ungrab(PopupUngrabStrategy::All);
            }
            self.refresh_popup_grab();
            input::reset_keyboard(self);
        }
        self.unbind_following_panes(id);
        if focused {
            self.restore_focus();
        }
    }

    /// After the focused window stopped being shown or drawn, focus a window
    /// a pane shows; without one, keep the focus, or take the newest window
    /// when nothing has it.
    pub(crate) fn restore_focus(&mut self) {
        let next = self
            .attachments
            .windows()
            .find(|&id| self.windows.catalog.drawable(id))
            .or_else(|| {
                self.windows
                    .catalog
                    .focused()
                    .or_else(|| self.windows.catalog.newest())
            });
        if let Some(next) = next {
            self.focus_window(next);
        }
    }

    pub(crate) fn remove_window(&mut self, id: WindowId) {
        self.unmap_window(id);
        self.cancel_window_activations(id);
        for pane in self.attachments.showing(id).to_vec() {
            self.remove_pane(pane);
            self.emit(pane, ServerToPane::Release("window closed".into()));
        }
        if let Some(window) = self.windows.by_id.remove(&id) {
            let mut surfaces = Vec::new();
            window.with_surfaces(|surface, _| surfaces.push(surface.clone()));
            for surface in &surfaces {
                self.windows.surface_cache.forget(surface);
            }
            self.windows
                .id_by_surface
                .remove(window.surface.wl_surface());
        }
        self.windows.catalog.remove(id);
        self.windows.popups.cleanup();
        self.windows.popups_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use wayland_client::Proxy;

    use crate::fixture::Fixture;

    #[test]
    fn mapping_a_toplevel_gives_it_keyboard_focus() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        let window = fixture.window(&toplevel);
        assert!(!fixture.state.windows.catalog.drawable(window));

        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);

        assert!(fixture.state.windows.catalog.drawable(window));
        assert_eq!(fixture.state.windows.catalog.focused(), Some(window));
        assert_eq!(fixture.app.keyboard_focus, Some(toplevel.surface.id()));
    }

    #[test]
    fn unchanged_catalog_focus_restores_keyboard_after_reset() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        let window = fixture.window(&toplevel);
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        crate::input::reset_keyboard(&mut fixture.state);
        fixture.roundtrip();
        assert_eq!(fixture.app.keyboard_focus, None);
        fixture.state.focus_window(window);
        fixture.roundtrip();
        assert_eq!(fixture.app.keyboard_focus, Some(toplevel.surface.id()));
        toplevel.surface.attach(None, 0, 0);
        toplevel.surface.commit();
        fixture.roundtrip();
        assert_eq!(fixture.app.keyboard_focus, None);
    }

    #[test]
    fn remapping_and_unchanged_state_requests_receive_configures() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        toplevel.surface.attach(None, 0, 0);
        toplevel.surface.commit();
        fixture.roundtrip();
        let before = fixture.app.configures.len();
        toplevel.surface.attach(None, 0, 0);
        toplevel.surface.commit();
        fixture.roundtrip();
        assert!(fixture.app.configures.len() > before);
        fixture.ack_configure(&toplevel);
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let before = fixture.app.configures.len();
        toplevel.role.unset_fullscreen();
        fixture.roundtrip();
        assert!(fixture.app.configures.len() > before);
        toplevel.role.set_maximized();
        fixture.roundtrip();
        let before = fixture.app.configures.len();
        toplevel.role.set_maximized();
        fixture.roundtrip();
        assert!(fixture.app.configures.len() > before);
        toplevel.role.set_fullscreen(None);
        fixture.roundtrip();
        let before = fixture.app.configures.len();
        toplevel.role.set_fullscreen(None);
        fixture.roundtrip();
        assert!(fixture.app.configures.len() > before);
    }

    #[test]
    fn destroyed_toplevels_leave_no_cached_surface_state() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let _child = fixture.subsurface(&toplevel.surface, [255, 0, 0, 255]);
        assert_eq!(fixture.state.windows.surface_cache.snapshots().len(), 2);

        toplevel.role.destroy();
        toplevel.xdg.destroy();
        fixture.roundtrip();
        assert_eq!(fixture.state.windows.surface_cache.surfaces().count(), 0);
    }

    #[test]
    fn a_reparented_subsurface_belongs_to_its_new_window() {
        use smithay::reexports::wayland_server::Resource as _;
        use wayland_client::Proxy as _;

        let mut fixture = Fixture::new();
        let first = fixture.toplevel("first");
        fixture.present(&first.surface, 4, 4, [0, 0, 0, 255]);
        let second = fixture.toplevel("second");
        fixture.present(&second.surface, 4, 4, [0, 0, 0, 255]);
        let (child, role) = fixture.subsurface(&first.surface, [255, 0, 0, 255]);
        let id = child.id().protocol_id();
        let owner = |fixture: &Fixture| {
            let surface = fixture
                .state
                .windows
                .surface_cache
                .snapshots()
                .keys()
                .find(|surface| surface.id().protocol_id() == id)
                .expect("the child has pixels")
                .clone();
            fixture.state.window_for_surface(&surface)
        };
        assert_eq!(owner(&fixture), Some(fixture.window(&first)));

        role.destroy();
        let _role = fixture.place_subsurface(&child, &second.surface, [0, 255, 0, 255]);
        assert_eq!(owner(&fixture), Some(fixture.window(&second)));
    }

    #[test]
    fn a_readback_finishing_after_its_surface_was_forgotten_keeps_no_spare() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let surface = fixture.state.windows.by_id[&fixture.window(&toplevel)]
            .surface
            .wl_surface()
            .clone();
        let cache = &mut fixture.state.windows.surface_cache;
        cache.forget(&surface);
        cache.return_spare(&surface, crate::Snapshot::single_pixel([0, 0, 0, 255]));
        assert_eq!(cache.surfaces().count(), 0);
    }
}
