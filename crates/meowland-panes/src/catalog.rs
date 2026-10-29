//! Window identity, mapping order, selection and focus without Wayland
//! resources.

use std::collections::HashMap;

use meowland_protocol::{Show, WindowId, WindowInfo, WindowState};

#[derive(Debug)]
struct Metadata {
    app_id: String,
    title: String,
    mapped_at: Option<u64>,
}

/// Window identities, mapping order, focus, and selection policy.
///
/// The catalog deliberately knows nothing about Wayland resources, so
/// selection and focus can be exercised without a display.
#[derive(Debug, Default)]
pub struct WindowCatalog {
    windows: HashMap<WindowId, Metadata>,
    next: u64,
    mapping_order: u64,
    newest: Option<WindowId>,
    focused: Option<WindowId>,
}

impl WindowCatalog {
    /// Register an unmapped window and return its identity.
    pub fn insert(&mut self, app_id: String, title: String) -> WindowId {
        self.next += 1;
        let id = WindowId::from(self.next);
        self.windows.insert(
            id,
            Metadata {
                app_id,
                title,
                mapped_at: None,
            },
        );
        id
    }

    /// Replace a registered window's application ID and title.
    pub fn update(&mut self, id: WindowId, app_id: String, title: String) {
        if let Some(window) = self.windows.get_mut(&id) {
            window.app_id = app_id;
            window.title = title;
        }
    }

    /// Mark a window as drawable, returning whether that changed.
    pub fn map(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        if window.mapped_at.is_some() {
            return false;
        }
        self.mapping_order += 1;
        window.mapped_at = Some(self.mapping_order);
        self.newest = Some(id);
        true
    }

    /// Mark a window as no longer drawable, returning whether that changed.
    pub fn unmap(&mut self, id: WindowId) -> bool {
        let changed = self
            .windows
            .get_mut(&id)
            .and_then(|window| window.mapped_at.take())
            .is_some();
        if self.newest == Some(id) {
            self.newest = self.newest_matching(|_| true);
        }
        if self.focused == Some(id) {
            self.focused = None;
        }
        changed
    }

    /// Whether the window is mapped and may be shown in a pane.
    pub fn drawable(&self, id: WindowId) -> bool {
        self.windows
            .get(&id)
            .is_some_and(|window| window.mapped_at.is_some())
    }

    /// Most recently mapped drawable window.
    pub const fn newest(&self) -> Option<WindowId> {
        self.newest
    }

    /// Window that currently has focus.
    pub const fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// Focus a drawable window and return the previously focused window.
    pub fn focus(&mut self, window: Option<WindowId>) -> Option<WindowId> {
        let window = window.filter(|&id| self.drawable(id));
        std::mem::replace(&mut self.focused, window)
    }

    /// Public description of a window for pane clients.
    pub fn info(&self, id: WindowId) -> WindowInfo {
        let (app_id, title) = self
            .windows
            .get(&id)
            .map_or_default(|window| (window.app_id.clone(), window.title.clone()));
        WindowInfo {
            id,
            app_id,
            title,
            state: if self.focused == Some(id) {
                WindowState::Active
            } else {
                WindowState::Inactive
            },
        }
    }

    /// Current title of a window, or an empty string when it is unknown.
    pub fn title(&self, id: WindowId) -> &str {
        self.windows
            .get(&id)
            .map_or("", |window| window.title.as_str())
    }

    /// Resolve a pane selector to a drawable window.
    pub fn select(&self, show: &Show, activation: Option<WindowId>) -> Option<WindowId> {
        let window = match show {
            Show::Id(id) => Some(*id),
            Show::Newest => self.newest,
            Show::Focused => self.focused,
            Show::AppId(app_id) => self.newest_matching(|window| window.app_id == *app_id),
            Show::Activation(_) => activation,
        }?;
        self.drawable(window).then_some(window)
    }

    /// Forget a window entirely.
    pub fn remove(&mut self, window: WindowId) {
        self.unmap(window);
        self.windows.remove(&window);
    }

    fn newest_matching(&self, predicate: impl Fn(&Metadata) -> bool) -> Option<WindowId> {
        self.windows
            .iter()
            .filter(|(_, window)| predicate(window))
            .filter_map(|(id, window)| window.mapped_at.map(|order| (order, *id)))
            .max_by_key(|(order, _)| *order)
            .map(|(_, id)| id)
    }
}
