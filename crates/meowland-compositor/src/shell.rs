//! XDG shell protocol handlers and popup placement.

use std::time::Instant;

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy,
    find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::input::Seat;
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::utils::{Logical, Rectangle, Serial};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};

use super::{State, Window, WlSurface, input, render};

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
                callback_pending: false,
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

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Some(seat) = Seat::from_resource(&seat) else {
            dismiss_popup(self, &root, &kind);
            return;
        };
        if seat != self.seat || !input::valid_serial(self, serial, &root) {
            dismiss_popup(self, &root, &kind);
            return;
        }
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        let previous = grab.previous_serial().unwrap_or(serial);
        let keyboard = seat.get_keyboard();
        let pointer = seat.get_pointer();
        let keyboard_allowed = keyboard.as_ref().is_none_or(|keyboard| {
            !keyboard.is_grabbed() || keyboard.has_grab(serial) || keyboard.has_grab(previous)
        });
        let pointer_allowed = pointer.as_ref().is_none_or(|pointer| {
            !pointer.is_grabbed() || pointer.has_grab(serial) || pointer.has_grab(previous)
        });
        if !keyboard_allowed || !pointer_allowed {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }
        if let Some(keyboard) = keyboard {
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = pointer {
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
        self.popup_grab = Some(grab);
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.touch(window);
        }
        self.refresh_popup_grab();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        self.remove_window(id);
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
        self.toplevels.update(id, &info.title, &info.app_id);
        self.send_titles(id);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }
}

fn dismiss_popup(state: &mut State, root: &WlSurface, popup: &PopupKind) {
    if let Some(window) = state.window_for_surface(popup.wl_surface()) {
        state.touch(window);
    }
    let _ = PopupManager::dismiss_popup(root, popup);
}

fn metadata(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map_or_default(|data| {
                let data = data.lock().expect("xdg toplevel state lock poisoned");
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
