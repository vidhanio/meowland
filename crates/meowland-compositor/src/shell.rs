//! XDG shell protocol handlers and popup placement.

use std::time::Instant;

use smithay::{
    desktop::{PopupKind, get_popup_toplevel_coords},
    utils::{Logical, Rectangle},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use super::{Event, Policy, State, Window, WlSurface, input, render};

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let (app_id, title) = metadata(&surface);
        let id = self.catalog.insert(app_id, title);
        self.ids.insert(surface.wl_surface().clone(), id);
        self.windows.insert(
            id,
            Window {
                surface,
                fullscreen: false,
                callback_due: Instant::now(),
            },
        );
        self.apply_window_state(id);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        if self
            .popups
            .track_popup(PopupKind::Xdg(surface.clone()))
            .is_err()
        {
            return;
        }
        place_popup(self, &surface, positioner);
        let _ = surface.send_configure();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        place_popup(self, &surface, positioner);
        surface.send_repositioned(token);
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        let stale: Vec<WlSurface> = self
            .snapshots
            .keys()
            .filter(|surface| self.window_for_surface(surface) == Some(id))
            .cloned()
            .collect();
        for surface in stale {
            self.snapshots.remove(&surface);
        }
        self.ids.remove(surface.wl_surface());
        self.cancel_window_activations(id);
        if self.keyboard_pane.is_some_and(|pane| {
            self.attachments
                .panes
                .get(&pane)
                .is_some_and(|entry| entry.window() == Some(id))
        }) {
            input::reset_keyboard(self);
        }
        for pane in self.panes_showing(id) {
            if self.attachments.panes[&pane].policy == Policy::FollowNewest {
                if let Some(entry) = self.attachments.panes.get_mut(&pane) {
                    entry.clear();
                }
                let _ = self.events.send(Event::Title {
                    pane,
                    title: String::new(),
                });
            } else {
                self.remove_pane(pane);
                let _ = self.events.send(Event::Release {
                    pane,
                    reason: "window closed".into(),
                });
            }
        }
        self.leave_output(id);
        self.windows.remove(&id);
        self.attachments.forget_window(id);
        let was_newest = self.catalog.newest() == Some(id);
        self.catalog.remove(id);
        if was_newest && let Some(newest) = self.catalog.newest() {
            self.follow_panes(newest);
        }
        self.popups.cleanup();
        if self.catalog.focused() == Some(id) {
            if let Some(next) = self.catalog.newest() {
                self.focus_window(next);
            } else {
                self.catalog.focus(None);
                let _ = self.events.send(Event::Focus(None));
            }
        }
        let _ = self.events.send(Event::WindowDown(id));
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        window.fullscreen = true;
        if self.catalog.drawable(id) {
            self.follow_panes(id);
        }
        self.enter_output(id);
        self.focus_window(id);
        self.apply_window_state(id);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&id) {
            window.fullscreen = false;
        }
        self.apply_window_state(id);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.ids.get(surface.wl_surface()).copied() {
            self.apply_window_state(id);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        let (app_id, title) = metadata(&surface);
        self.catalog.update(id, app_id, title);
        self.resolve_pending_panes();
        let info = self.window_info(id);
        let _ = self.events.send(Event::WindowUp(info));
        self.send_titles(id);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }
}

fn metadata(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map_or_default(|data| {
                let data = data.lock().unwrap();
                (
                    data.app_id.clone().unwrap_or_default(),
                    data.title.clone().unwrap_or_default(),
                )
            })
    })
}

fn popup_target(state: &State, popup: &PopupSurface) -> Rectangle<i32, Logical> {
    let window = state.window_for_surface(popup.wl_surface());
    let geometry = window
        .and_then(|window| state.windows.get(&window))
        .map_or_else(
            || (0, 0).into(),
            |window| render::window_geometry(window.surface.wl_surface()),
        );
    let parent = get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
    let size = window.map_or(state.mode, |window| state.pane_size(window));
    Rectangle::new(
        (
            geometry.x.saturating_add(parent.x).saturating_neg(),
            geometry.y.saturating_add(parent.y).saturating_neg(),
        )
            .into(),
        size,
    )
}

fn place_popup(state: &State, surface: &PopupSurface, positioner: PositionerState) {
    let target = popup_target(state, surface);
    surface.with_pending_state(|pending| {
        pending.geometry = positioner.get_unconstrained_geometry(target);
        pending.positioner = positioner;
    });
}
