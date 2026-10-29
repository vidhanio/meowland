//! Standard Wayland discovery for mapped application windows.

use std::collections::HashMap;

use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::foreign_toplevel_list::{
        ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
    },
};

use super::{Compositor, WindowId};

pub(crate) struct Toplevels {
    protocol: ForeignToplevelListState,
    handles: HashMap<WindowId, ForeignToplevelHandle>,
}

impl Toplevels {
    pub(crate) fn new(display: &DisplayHandle) -> Self {
        Self {
            protocol: ForeignToplevelListState::new::<Compositor>(display),
            handles: HashMap::new(),
        }
    }

    pub(crate) fn announce(&mut self, id: WindowId, title: &str, app_id: &str) {
        let handle = self.protocol.new_toplevel::<Compositor>(title, app_id);
        if let Some(previous) = self.handles.insert(id, handle) {
            previous.send_closed();
        }
    }

    pub(crate) fn update(&self, id: WindowId, title: &str, app_id: &str) {
        if let Some(handle) = self.handles.get(&id) {
            handle.send_title(title);
            handle.send_app_id(app_id);
            handle.send_done();
        }
    }

    pub(crate) fn remove(&mut self, id: WindowId) {
        if let Some(handle) = self.handles.remove(&id) {
            handle.send_closed();
        }
    }
}

impl ForeignToplevelListHandler for Compositor {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.protocols.toplevels.protocol
    }
}
