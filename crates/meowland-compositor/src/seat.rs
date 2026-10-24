//! Seat focus, cursor delivery, and selection protocol integration.

use smithay::{
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        pointer::{CursorIcon, CursorImageStatus, PointerHandle},
        tablet::TabletSeatHandler,
    },
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
            wlr_data_control::{
                DataControlHandler as WlrDataControlHandler,
                DataControlState as WlrDataControlState,
            },
        },
    },
};

use super::{Event, Pane, PaneId, State, WindowId, clipboard, input};

impl State {
    pub fn focus_window(&mut self, window: WindowId) {
        self.refresh_popup_grab();
        if self.catalog.focused() == Some(window) {
            return;
        }
        if let Some(grab) = &mut self.popup_grab {
            grab.ungrab(smithay::desktop::PopupUngrabStrategy::All);
            self.refresh_popup_grab();
        }
        input::reset_keyboard(self);
        self.recent_serials.clear();
        let previous = self.catalog.focus(Some(window));
        let surface = self
            .windows
            .get(&window)
            .map(|entry| entry.surface.wl_surface().clone());
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
            self.keyboard = Some(keyboard);
        }
        if let Some(previous) = previous {
            self.apply_window_state(previous);
        }
        self.apply_window_state(window);
        let _ = self.events.send(Event::Focus(Some(window)));
    }

    pub fn send_pane_cursor(&mut self, pane: PaneId) {
        let window = self.attachments.panes.get(&pane).and_then(Pane::window);
        let focused = self
            .pointer
            .as_ref()
            .and_then(PointerHandle::current_focus)
            .and_then(|surface| self.window_for_surface(&surface));
        let shape = if window.is_some() && window == focused {
            self.cursor.clone()
        } else {
            None
        };
        self.send_cursor(pane, shape);
    }

    fn send_cursor(&mut self, pane: PaneId, shape: Option<String>) {
        let cursor = (pane, shape);
        if self.sent_cursor.as_ref() == Some(&cursor) {
            return;
        }
        let _ = self.events.send(Event::Cursor {
            pane,
            shape: cursor.1.clone(),
        });
        self.sent_cursor = Some(cursor);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(Resource::client);
        set_data_device_focus(&self.display, seat, client.clone());
        set_primary_focus(&self.display, seat, client);
    }

    /// During pointer motion, delivery waits until the caller restores the
    /// pointer handle so the destination can be checked against its focus.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let shape = match image {
            CursorImageStatus::Named(icon) => Some(cursor_name(icon).to_owned()),
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => None,
        };
        self.cursor = shape;
        if self.pointer.is_some()
            && let Some(pane) = self.cursor_pane
        {
            self.send_pane_cursor(pane);
        }
    }
}

/// CSS cursor names, which is what the kitty pointer-shape protocol takes.
const fn cursor_name(icon: CursorIcon) -> &'static str {
    match icon {
        CursorIcon::ContextMenu => "context-menu",
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::Move => "move",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize => "ew-resize",
        CursorIcon::NsResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ColResize => "col-resize",
        CursorIcon::RowResize => "row-resize",
        CursorIcon::AllScroll => "all-scroll",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        CursorIcon::DndAsk => "dnd-ask",
        _ => "default",
    }
}

impl SelectionHandler for State {
    type SelectionUserData = clipboard::Source;

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        clipboard::selection(self, target, source);
    }

    fn send_selection(
        &mut self,
        _target: SelectionTarget,
        mime: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        source: &Self::SelectionUserData,
    ) {
        clipboard::send(self, fd, &mime, source);
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection
    }
}

impl WlrDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.data_control
    }
}

impl ExtDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control
    }
}

impl PointerConstraintsHandler for State {}
impl DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {}
impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}
impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device
    }
}

impl OutputHandler for State {}
