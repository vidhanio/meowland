//! Window identity, mapping order, selection and focus without Wayland
//! resources.

use std::collections::HashMap;

use meowland_protocol::{Show, WindowInfo};

use super::WindowId;

struct Metadata {
    app_id: String,
    title: String,
    mapped_at: Option<u64>,
}

#[derive(Default)]
pub(crate) struct WindowCatalog {
    windows: HashMap<WindowId, Metadata>,
    next: u64,
    mapping_order: u64,
    newest: Option<WindowId>,
    focused: Option<WindowId>,
}

impl WindowCatalog {
    pub(crate) fn insert(&mut self, app_id: String, title: String) -> WindowId {
        self.next += 1;
        let id = WindowId(self.next);
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

    pub(crate) fn update(&mut self, id: WindowId, app_id: String, title: String) {
        if let Some(window) = self.windows.get_mut(&id) {
            window.app_id = app_id;
            window.title = title;
        }
    }

    pub(crate) fn map(&mut self, id: WindowId) -> bool {
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

    pub(crate) fn unmap(&mut self, id: WindowId) -> bool {
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

    pub(crate) fn drawable(&self, id: WindowId) -> bool {
        self.windows
            .get(&id)
            .is_some_and(|window| window.mapped_at.is_some())
    }

    pub(crate) const fn newest(&self) -> Option<WindowId> {
        self.newest
    }

    pub(crate) const fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    pub(crate) fn focus(&mut self, window: Option<WindowId>) -> Option<WindowId> {
        let window = window.filter(|&id| self.drawable(id));
        std::mem::replace(&mut self.focused, window)
    }

    pub(crate) fn info(&self, id: WindowId) -> WindowInfo {
        let (app_id, title) = self
            .windows
            .get(&id)
            .map_or_default(|window| (window.app_id.clone(), window.title.clone()));
        WindowInfo {
            id: id.0,
            app_id,
            title,
            active: self.focused == Some(id),
        }
    }

    pub(crate) fn title(&self, id: WindowId) -> &str {
        self.windows
            .get(&id)
            .map_or("", |window| window.title.as_str())
    }

    pub(crate) fn select(&self, show: &Show, activation: Option<WindowId>) -> Option<WindowId> {
        let window = match show {
            Show::Id(id) => Some(WindowId(*id)),
            Show::Newest => self.newest,
            Show::Focused => self.focused,
            Show::AppId(app_id) => self.newest_matching(|window| window.app_id == *app_id),
            Show::Activation(_) => activation,
        }?;
        self.drawable(window).then_some(window)
    }

    pub(crate) fn remove(&mut self, window: WindowId) {
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
