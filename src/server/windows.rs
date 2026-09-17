//! The server's copy of what the compositor has told it about its windows.

use std::collections::HashMap;

use crate::protocol::{WindowId, control};

/// The windows the server knows about, for the control socket to answer with.
///
/// The compositor owns its windows; this is what it last said about them, so
/// that `meowland list` and attaching by ID need no round trip. Nothing else
/// reads it, and nothing but the compositor's messages writes it.
#[derive(Debug, Default)]
pub struct Windows {
    windows: HashMap<WindowId, WindowInfo>,
    active: Option<WindowId>,
}

#[derive(Debug, Default, Clone)]
struct WindowInfo {
    label: Option<String>,
    title: Option<String>,
}

impl Windows {
    /// A window exists under these names, or has been renamed.
    pub fn named(&mut self, window: WindowId, label: Option<String>, title: Option<String>) {
        let known = self.windows.entry(window).or_default();
        known.label = label;
        known.title = title;
    }

    /// The window that has the keyboard changed.
    pub const fn focused(&mut self, window: Option<WindowId>) {
        self.active = window;
    }

    /// A window is gone.
    pub fn closed(&mut self, window: WindowId) {
        self.windows.remove(&window);
        if self.active == Some(window) {
            self.active = None;
        }
    }

    /// Whether the compositor has this window.
    pub fn has(&self, window: WindowId) -> bool {
        self.windows.contains_key(&window)
    }

    /// The windows, oldest first, as the control socket lists them.
    pub fn list(&self) -> Vec<control::Window> {
        let mut windows: Vec<_> = self
            .windows
            .iter()
            .map(|(id, info)| control::Window {
                id: *id,
                label: info.label.clone().unwrap_or_default(),
                title: info.title.clone().unwrap_or_default(),
                active: self.active == Some(*id),
            })
            .collect();
        windows.sort_by_key(|window| window.id);
        windows
    }
}
