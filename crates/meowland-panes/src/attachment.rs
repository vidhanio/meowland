//! Pane bindings, independent of Wayland resources.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};

use meowland_protocol::{Follow, Target};

use crate::{PaneId, WindowId};

/// A pane attached to a window selector, carrying host state `P`.
pub struct AttachedPane<P> {
    target: Target,
    window: Option<WindowId>,
    /// How long the pane keeps matching its selection.
    pub follow: Follow,
    /// Host-owned state of the pane, such as its frame presentation.
    pub state: P,
}

impl<P> AttachedPane<P> {
    /// Window the pane currently shows, if any.
    pub const fn window(&self) -> Option<WindowId> {
        self.window
    }

    /// Target this pane is waiting to resolve while showing no window.
    pub const fn waiting(&self) -> Option<&Target> {
        match self.window {
            None => Some(&self.target),
            Some(_) => None,
        }
    }
}

impl<P> fmt::Debug for AttachedPane<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachedPane")
            .field("target", &self.target)
            .field("window", &self.window)
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

/// Attached panes plus the index from windows to the panes showing them.
///
/// Binding changes go through this type so the index cannot drift. Each
/// window lists its panes with the owner first: the pane whose size the
/// window takes, which is the one that most recently routed input to it.
pub struct Attachments<P> {
    panes: BTreeMap<PaneId, AttachedPane<P>>,
    by_window: HashMap<WindowId, Vec<PaneId>>,
}

impl<P> Default for Attachments<P> {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
            by_window: HashMap::new(),
        }
    }
}

impl<P> fmt::Debug for Attachments<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Attachments")
            .field("panes", &self.panes.len())
            .field("windows", &self.by_window.len())
            .finish_non_exhaustive()
    }
}

impl<P> Attachments<P> {
    /// Number of attached panes.
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    /// Whether no pane is attached.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Whether a pane is attached.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
    }

    /// Look up an attached pane.
    pub fn get(&self, pane: PaneId) -> Option<&AttachedPane<P>> {
        self.panes.get(&pane)
    }

    /// Look up an attached pane's host state for mutation.
    pub fn state_mut(&mut self, pane: PaneId) -> Option<&mut P> {
        self.panes.get_mut(&pane).map(|entry| &mut entry.state)
    }

    /// Iterate over attached panes in pane-identity order.
    pub fn iter(&self) -> impl Iterator<Item = (PaneId, &AttachedPane<P>)> {
        self.panes.iter().map(|(&pane, entry)| (pane, entry))
    }

    /// Visit every attached pane's host state in pane-identity order,
    /// together with the window it shows.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (PaneId, Option<WindowId>, &mut P)> {
        self.panes
            .iter_mut()
            .map(|(&pane, entry)| (pane, entry.window, &mut entry.state))
    }

    /// Attach a pane waiting for its target, replacing any previous
    /// attachment with the same ID.
    pub fn insert(&mut self, pane: PaneId, target: Target, follow: Follow, state: P) {
        let entry = AttachedPane {
            target,
            window: None,
            follow,
            state,
        };
        if let Some(window) = self.panes.insert(pane, entry).and_then(|old| old.window) {
            self.unlink(window, pane);
        }
    }

    /// Replace the target a pane matches against once it next waits.
    pub fn reselect(&mut self, pane: PaneId, target: Target) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.target = target;
        }
    }

    /// Bind a pane to a window, returning the activation token it was waiting
    /// for. A window already shown keeps its owner.
    pub fn show(&mut self, pane: PaneId, window: WindowId) -> Option<String> {
        let entry = self.panes.get_mut(&pane)?;
        let previous = entry.window.replace(window);
        let token = match (&entry.target, previous) {
            (Target::Activation(token), None) => Some(token.clone()),
            _ => None,
        };
        if previous != Some(window) {
            if let Some(previous) = previous {
                self.unlink(previous, pane);
            }
            self.by_window.entry(window).or_default().push(pane);
        }
        token
    }

    /// Leave a pane attached but showing no window, waiting on its target.
    pub fn unbind(&mut self, pane: PaneId) {
        if let Some(window) = self
            .panes
            .get_mut(&pane)
            .and_then(|entry| entry.window.take())
        {
            self.unlink(window, pane);
        }
    }

    /// Detach a pane and return its state.
    pub fn remove(&mut self, pane: PaneId) -> Option<AttachedPane<P>> {
        let removed = self.panes.remove(&pane)?;
        if let Some(window) = removed.window {
            self.unlink(window, pane);
        }
        Some(removed)
    }

    /// Panes showing a window, owner first.
    pub fn showing(&self, window: WindowId) -> &[PaneId] {
        self.by_window.get(&window).map_or(&[], Vec::as_slice)
    }

    /// Pane whose size the window takes.
    pub fn owner(&self, window: WindowId) -> Option<PaneId> {
        self.showing(window).first().copied()
    }

    /// Make a pane showing a window its owner, returning whether the owner
    /// changed.
    pub fn decide(&mut self, window: WindowId, pane: PaneId) -> bool {
        let Some(panes) = self.by_window.get_mut(&window) else {
            return false;
        };
        match panes.iter().position(|&shown| shown == pane) {
            Some(0) | None => false,
            Some(index) => {
                panes[..=index].rotate_right(1);
                true
            }
        }
    }

    /// Windows shown by attached panes, in pane-identity order.
    pub fn windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.panes.values().filter_map(AttachedPane::window)
    }

    fn unlink(&mut self, window: WindowId, pane: PaneId) {
        if let Some(panes) = self.by_window.get_mut(&window) {
            panes.retain(|&shown| shown != pane);
            if panes.is_empty() {
                self.by_window.remove(&window);
            }
        }
    }
}
