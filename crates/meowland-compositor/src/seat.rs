//! Seat focus, cursor delivery, and selection protocol integration.

use smithay::{
    backend::input::InputTime,
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        pointer::{CursorIcon, CursorImageStatus, MotionEvent, PointerHandle},
        tablet::TabletSeatHandler,
    },
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{SERIAL_COUNTER, Serial},
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

use super::{AttachedPane, Compositor, PaneId, ServerToPane, WindowId, clipboard, input};

/// Whether cursor images reach panes as soon as clients set them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CursorDelivery {
    #[default]
    Live,
    /// Pointer events are being dispatched; the dispatcher delivers the
    /// cursor of the final pointer focus afterwards.
    Deferred,
}

#[derive(Default)]
pub(crate) struct CursorState {
    pub shape: Option<String>,
    pub sent: Option<(PaneId, Option<String>)>,
    delivery: CursorDelivery,
}

impl Compositor {
    pub(crate) fn set_keyboard_focus(&mut self, focus: Option<WlSurface>, serial: Serial) {
        if let Some(keyboard) = self.input.keyboard.clone() {
            keyboard.set_focus(self, focus, serial);
            if keyboard.current_focus().is_none() {
                let seat = self.input.seat.clone();
                set_data_device_focus(&self.display, &seat, None);
                set_primary_focus(&self.display, &seat, None);
            }
        }
    }

    pub(crate) fn focus_window(&mut self, window: WindowId) {
        if !self.windows.catalog.drawable(window) {
            return;
        }
        self.refresh_popup_grab();
        if self.windows.catalog.focused() == Some(window) {
            let surface = self.windows.by_id[&window].surface.wl_surface().clone();
            if self.input.keyboard.as_ref().is_some_and(|keyboard| {
                !keyboard.is_grabbed() && keyboard.current_focus().as_ref() != Some(&surface)
            }) {
                self.set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
            }
            return;
        }
        if let Some(grab) = &mut self.input.popup_grab {
            grab.ungrab(smithay::desktop::PopupUngrabStrategy::All);
            self.refresh_popup_grab();
        }
        input::reset_keyboard(self);
        self.input.keyboard_state.forget_serials();
        let previous = self.windows.catalog.focus(Some(window));
        let surface = self
            .windows
            .by_id
            .get(&window)
            .map(|entry| entry.surface.wl_surface().clone());
        self.set_keyboard_focus(surface, SERIAL_COUNTER.next_serial());
        if let Some(previous) = previous {
            self.apply_window_state(previous);
        }
        self.apply_window_state(window);
    }

    /// Dispatch pointer events with cursor delivery deferred, so a cursor
    /// image set mid-dispatch is checked against the final pointer focus.
    pub(crate) fn dispatch_pointer(
        &mut self,
        dispatch: impl FnOnce(&mut Self, &PointerHandle<Self>),
    ) {
        let pointer = self.input.pointer.clone();
        self.input.cursor.delivery = CursorDelivery::Deferred;
        dispatch(self, &pointer);
        self.input.cursor.delivery = CursorDelivery::Live;
    }

    pub(crate) fn send_pane_cursor(&mut self, pane: PaneId) {
        let window = self.attachments.get(pane).and_then(AttachedPane::window);
        let focused = self
            .input
            .pointer
            .current_focus()
            .and_then(|surface| self.window_for_surface(&surface));
        let shape = if window.is_some() && window == focused {
            self.input.cursor.shape.clone()
        } else {
            None
        };
        self.send_cursor(pane, shape);
    }

    /// Drop pointer focus on a window that is going away.
    pub(crate) fn clear_window_pointer(&mut self, window: WindowId) {
        let focused = self
            .input
            .pointer
            .current_focus()
            .and_then(|surface| self.window_for_surface(&surface));
        if focused != Some(window) {
            return;
        }
        self.dispatch_pointer(|state, pointer| {
            pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), InputTime::now());
            pointer.motion(
                state,
                None,
                &MotionEvent {
                    location: (0.0, 0.0).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: InputTime::now(),
                },
            );
            pointer.frame(state);
        });
        if let Some(pane) = self.input.last_input_pane {
            self.send_pane_cursor(pane);
        }
    }

    fn send_cursor(&mut self, pane: PaneId, shape: Option<String>) {
        let cursor = (pane, shape);
        if self.input.cursor.sent.as_ref() == Some(&cursor) {
            return;
        }
        self.emit(pane, ServerToPane::Cursor(cursor.1.clone()));
        self.input.cursor.sent = Some(cursor);
    }
}

impl SeatHandler for Compositor {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.input.seats
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(Resource::client);
        set_data_device_focus(&self.display, seat, client.clone());
        set_primary_focus(&self.display, seat, client);
    }

    /// During pointer dispatch, delivery waits until the dispatcher checks
    /// the destination against the final focus.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let shape = match image {
            CursorImageStatus::Named(icon) => Some(cursor_name(icon).to_owned()),
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => None,
        };
        self.input.cursor.shape = shape;
        if self.input.cursor.delivery == CursorDelivery::Live
            && let Some(pane) = self.input.last_input_pane
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

impl SelectionHandler for Compositor {
    type SelectionUserData = clipboard::Text;

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        self.copy_selection(target, source);
    }

    fn send_selection(
        &mut self,
        _target: SelectionTarget,
        _mime: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        source: &Self::SelectionUserData,
    ) {
        self.send_text(fd, source);
    }
}

impl PrimarySelectionHandler for Compositor {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.protocols.primary_selection
    }
}

impl WlrDataControlHandler for Compositor {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.protocols.data_control
    }
}

impl ExtDataControlHandler for Compositor {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.protocols.ext_data_control
    }
}

impl PointerConstraintsHandler for Compositor {}
impl DndGrabHandler for Compositor {}
impl WaylandDndGrabHandler for Compositor {}
impl TabletSeatHandler for Compositor {
    type ToolFocus = WlSurface;
}
impl DataDeviceHandler for Compositor {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.protocols.data_device
    }
}

impl OutputHandler for Compositor {}
