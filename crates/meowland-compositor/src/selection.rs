//! Compositor hooks for the terminal selection bridge.

use std::{io, os::fd::OwnedFd};

use meowland_panes::PaneId;
use meowland_protocol::{ServerToPane, clipboard::Transport};
use smithay::wayland::selection::{
    SelectionTarget,
    data_device::{request_data_device_client_selection, set_data_device_selection},
    primary_selection::{request_primary_client_selection, set_primary_selection},
};

use super::{Compositor, input};

impl meowland_selection::Host for Compositor {
    fn handle(&self) -> &meowland_loop::Handle<Self> {
        &self.handle
    }

    fn bridge(&mut self) -> &mut meowland_selection::Bridge<Self> {
        &mut self.clipboard
    }

    fn emit(&mut self, pane: PaneId, message: ServerToPane) {
        Self::emit(self, pane, message);
    }

    fn selection_pane(&self) -> Option<PaneId> {
        let focused = self.windows.catalog.focused();
        self.input
            .last_input_pane
            .filter(|pane| {
                self.attachments
                    .get(*pane)
                    .is_some_and(|pane| pane.window().is_some() && pane.window() == focused)
            })
            .or_else(|| focused.and_then(|focused| self.attachments.first_showing(focused)))
    }

    fn pane_transport(&self, pane: PaneId) -> Option<Transport> {
        self.attachments.get(pane).map(|pane| pane.clipboard)
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        self.attachments.contains(pane)
    }

    fn pane_has_window(&self, pane: PaneId) -> bool {
        self.attachments
            .get(pane)
            .is_some_and(|pane| pane.window().is_some())
    }

    fn request_selection(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) -> io::Result<()> {
        match target {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.input.seat, mime, fd)
                    .map_err(io::Error::other)
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.input.seat, mime, fd)
                    .map_err(io::Error::other)
            }
        }
    }

    fn set_selection(
        &mut self,
        target: SelectionTarget,
        mimes: Vec<String>,
        source: meowland_selection::Source,
    ) {
        match target {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display, &self.input.seat, mimes, source);
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display, &self.input.seat, mimes, source);
            }
        }
    }

    fn paste(&mut self, pane: PaneId) {
        input::paste_action(self, pane);
    }
}
