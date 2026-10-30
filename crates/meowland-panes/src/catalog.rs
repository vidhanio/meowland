//! Window identity, mapping order, selection and focus without Wayland
//! resources.

use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
};

use meowland_protocol::{WindowInfo, WindowState};

use crate::{Matcher, WindowId};

const ID_SUFFIX_LEN: usize = 4;
const ID_SUFFIX_ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

#[derive(Debug)]
struct Metadata {
    id: Option<String>,
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
    suffixes: RandomState,
    salt: u64,
}

impl WindowCatalog {
    /// Register an unmapped window and return its identity.
    pub fn insert(&mut self, app_id: String, title: String) -> WindowId {
        self.next += 1;
        let id = WindowId::from(self.next);
        self.windows.insert(
            id,
            Metadata {
                id: None,
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
    ///
    /// The first map assigns the window's unique ID from its application ID
    /// at that time.
    pub fn map(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.get(&id) else {
            return false;
        };
        if window.mapped_at.is_some() {
            return false;
        }
        let unique = window
            .id
            .is_none()
            .then(|| id_prefix(&window.app_id))
            .map(|prefix| self.unique_id(&prefix));
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        self.mapping_order += 1;
        if unique.is_some() {
            window.id = unique;
        }
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
        let (unique, app_id, title) = self.windows.get(&id).map_or_default(|window| {
            (
                window.id.clone().unwrap_or_default(),
                window.app_id.clone(),
                window.title.clone(),
            )
        });
        WindowInfo {
            id: unique,
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

    /// Current application ID of a window, or an empty string when it is
    /// unknown.
    pub fn app_id(&self, id: WindowId) -> &str {
        self.windows
            .get(&id)
            .map_or("", |window| window.app_id.as_str())
    }

    /// Most recently mapped drawable window matching a selector.
    pub fn select(&self, matcher: &Matcher) -> Option<WindowId> {
        self.newest_matching(|window| {
            matcher.matches(window.id.as_deref(), &window.app_id, &window.title)
        })
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

    fn unique_id(&mut self, prefix: &str) -> String {
        loop {
            self.salt += 1;
            let mut bits = self.suffixes.hash_one(self.salt);
            let mut id = format!("{prefix}_");
            for _ in 0..ID_SUFFIX_LEN {
                let digit = usize::try_from(bits % 36).expect("a base-36 digit fits in usize");
                id.push(char::from(ID_SUFFIX_ALPHABET[digit]));
                bits /= 36;
            }
            if !self
                .windows
                .values()
                .any(|window| window.id.as_deref() == Some(id.as_str()))
            {
                return id;
            }
        }
    }
}

fn id_prefix(app_id: &str) -> String {
    if app_id.is_empty() {
        return "window".to_owned();
    }
    app_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}
