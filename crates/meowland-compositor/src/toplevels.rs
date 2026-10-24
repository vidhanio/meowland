//! Standard Wayland discovery for mapped application windows.

use std::collections::HashMap;

use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::foreign_toplevel_list::{
        ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
    },
};

use super::{State, WindowId};

pub struct Toplevels {
    protocol: ForeignToplevelListState,
    handles: HashMap<WindowId, ForeignToplevelHandle>,
}

impl Toplevels {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            protocol: ForeignToplevelListState::new::<State>(display),
            handles: HashMap::new(),
        }
    }

    pub fn announce(&mut self, id: WindowId, title: &str, app_id: &str) {
        let handle = self.protocol.new_toplevel::<State>(title, app_id);
        if let Some(previous) = self.handles.insert(id, handle) {
            previous.send_closed();
        }
    }

    pub fn update(&self, id: WindowId, title: &str, app_id: &str) {
        if let Some(handle) = self.handles.get(&id) {
            handle.send_title(title);
            handle.send_app_id(app_id);
            handle.send_done();
        }
    }

    pub fn remove(&mut self, id: WindowId) {
        if let Some(handle) = self.handles.remove(&id) {
            handle.send_closed();
        }
    }
}

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.toplevels.protocol
    }
}
