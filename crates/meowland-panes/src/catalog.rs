//! Window identity, mapping order, selection and focus without Wayland
//! resources.

use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
};

use meowland_protocol::{Selector, WindowInfo, WindowState};

use crate::WindowId;

const ID_SUFFIX_LEN: usize = 4;
const ID_SUFFIX_ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Where a window is in its map/unmap lifecycle.
#[derive(Debug)]
enum Lifecycle {
    /// Created but never mapped, so it has no unique ID yet.
    Registered,
    /// Drawable; `order` ranks it among mapped windows by recency.
    Mapped { id: String, order: u64 },
    /// Mapped before and keeping its unique ID.
    Unmapped { id: String },
}

#[derive(Debug)]
struct Metadata {
    app_id: String,
    title: String,
    lifecycle: Lifecycle,
}

impl Metadata {
    fn id(&self) -> Option<&str> {
        match &self.lifecycle {
            Lifecycle::Registered => None,
            Lifecycle::Mapped { id, .. } | Lifecycle::Unmapped { id } => Some(id),
        }
    }

    const fn mapped_order(&self) -> Option<u64> {
        match self.lifecycle {
            Lifecycle::Mapped { order, .. } => Some(order),
            Lifecycle::Registered | Lifecycle::Unmapped { .. } => None,
        }
    }
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
                app_id,
                title,
                lifecycle: Lifecycle::Registered,
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
        let unique = match &window.lifecycle {
            Lifecycle::Mapped { .. } => return false,
            Lifecycle::Unmapped { id } => id.clone(),
            Lifecycle::Registered => {
                let prefix = id_prefix(&window.app_id);
                self.unique_id(&prefix)
            }
        };
        self.mapping_order += 1;
        let order = self.mapping_order;
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        window.lifecycle = Lifecycle::Mapped { id: unique, order };
        true
    }

    /// Mark a window as no longer drawable, returning whether that changed.
    pub fn unmap(&mut self, id: WindowId) -> bool {
        if self.focused == Some(id) {
            self.focused = None;
        }
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        match std::mem::replace(&mut window.lifecycle, Lifecycle::Registered) {
            Lifecycle::Mapped { id, .. } => {
                window.lifecycle = Lifecycle::Unmapped { id };
                true
            }
            lifecycle => {
                window.lifecycle = lifecycle;
                false
            }
        }
    }

    /// Whether the window is mapped and may be shown in a pane.
    pub fn drawable(&self, id: WindowId) -> bool {
        self.windows
            .get(&id)
            .is_some_and(|window| window.mapped_order().is_some())
    }

    /// Most recently mapped drawable window.
    pub fn newest(&self) -> Option<WindowId> {
        self.newest_matching(|_, _| true)
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

    /// Public description of a window for pane clients, once it has been
    /// mapped and therefore has a unique ID.
    pub fn info(&self, id: WindowId) -> Option<WindowInfo> {
        let window = self.windows.get(&id)?;
        Some(WindowInfo {
            id: window.id()?.to_owned(),
            app_id: window.app_id.clone(),
            title: window.title.clone(),
            state: if self.focused == Some(id) {
                WindowState::Active
            } else {
                WindowState::Inactive
            },
        })
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

    /// Most recently mapped drawable window whose unique ID, app ID, or title
    /// equals the selector's value.
    pub fn select(&self, selector: &Selector) -> Option<WindowId> {
        self.select_where(selector, |_| true)
    }

    /// [`Self::select`] among windows for which `available` holds.
    pub fn select_where(
        &self,
        selector: &Selector,
        available: impl Fn(WindowId) -> bool,
    ) -> Option<WindowId> {
        self.newest_matching(|id, window| {
            available(id)
                && match selector {
                    Selector::Id(unique) => window.id() == Some(unique.as_str()),
                    Selector::AppId(app_id) => window.app_id == *app_id,
                    Selector::Title(title) => window.title == *title,
                }
        })
    }

    /// Forget a window entirely.
    pub fn remove(&mut self, window: WindowId) {
        self.unmap(window);
        self.windows.remove(&window);
    }

    fn newest_matching(
        &self,
        predicate: impl Fn(WindowId, &Metadata) -> bool,
    ) -> Option<WindowId> {
        self.windows
            .iter()
            .filter(|(id, window)| predicate(**id, window))
            .filter_map(|(id, window)| window.mapped_order().map(|order| (order, *id)))
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
                .any(|window| window.id() == Some(id.as_str()))
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
