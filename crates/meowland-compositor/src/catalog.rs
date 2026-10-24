//! Window identity, mapping, selection and focus policy without Wayland
//! resources.

use std::collections::HashMap;

use meowland_protocol::{Show, WindowInfo};

use super::WindowId;

struct Metadata {
    app_id: String,
    title: String,
    drawable: bool,
}

#[derive(Default)]
pub struct WindowCatalog {
    windows: HashMap<WindowId, Metadata>,
    next: u64,
    newest: Option<WindowId>,
    focused: Option<WindowId>,
}

impl WindowCatalog {
    pub fn insert(&mut self, app_id: String, title: String) -> WindowId {
        self.next += 1;
        let id = WindowId(self.next);
        self.windows.insert(
            id,
            Metadata {
                app_id,
                title,
                drawable: false,
            },
        );
        id
    }

    pub fn update(&mut self, id: WindowId, app_id: String, title: String) {
        if let Some(window) = self.windows.get_mut(&id) {
            window.app_id = app_id;
            window.title = title;
        }
    }

    pub fn announce(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        if std::mem::replace(&mut window.drawable, true) {
            return false;
        }
        self.newest = Some(id);
        true
    }

    pub fn drawable(&self, id: WindowId) -> bool {
        self.windows.get(&id).is_some_and(|window| window.drawable)
    }

    pub const fn newest(&self) -> Option<WindowId> {
        self.newest
    }

    pub const fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    pub const fn focus(&mut self, window: Option<WindowId>) -> Option<WindowId> {
        std::mem::replace(&mut self.focused, window)
    }

    pub fn info(&self, id: WindowId) -> WindowInfo {
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

    pub fn title(&self, id: WindowId) -> &str {
        self.windows
            .get(&id)
            .map_or("", |window| window.title.as_str())
    }

    pub fn select(&self, show: &Show, activation: Option<WindowId>) -> Option<WindowId> {
        let window = match show {
            Show::Id(id) => Some(WindowId(*id)),
            Show::Newest => self.newest,
            Show::Focused => self.focused,
            Show::AppId(app_id) => self
                .windows
                .iter()
                .filter(|(_, window)| window.drawable && window.app_id == *app_id)
                .map(|(id, _)| *id)
                .max(),
            Show::Activation(_) => activation,
        }?;
        self.drawable(window).then_some(window)
    }

    pub fn remove(&mut self, window: WindowId) {
        self.windows.remove(&window);
        if self.newest == Some(window) {
            self.newest = self
                .windows
                .iter()
                .filter(|(_, entry)| entry.drawable)
                .map(|(id, _)| *id)
                .max();
        }
    }
}
