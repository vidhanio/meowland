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
    /// Window the pane currently owns, if any.
    pub const fn window(&self) -> Option<WindowId> {
        self.window
    }

    /// Target this pane is waiting to resolve while owning no window.
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

/// Attached panes plus the index from windows to the pane owning them.
///
/// A window is owned by at most one pane and a pane owns at most one window.
/// Binding changes go through this type so the index cannot drift.
pub struct Attachments<P> {
    panes: BTreeMap<PaneId, AttachedPane<P>>,
    owners: HashMap<WindowId, PaneId>,
}

impl<P> Default for Attachments<P> {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
            owners: HashMap::new(),
        }
    }
}

impl<P> fmt::Debug for Attachments<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Attachments")
            .field("panes", &self.panes.len())
            .field("windows", &self.owners.len())
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
    /// together with the window it owns.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (PaneId, Option<WindowId>, &mut P)> {
        self.panes
            .iter_mut()
            .map(|(&pane, entry)| (pane, entry.window, &mut entry.state))
    }

    /// Panes waiting for a window, in pane-identity order.
    pub fn waiting(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.panes
            .iter()
            .filter(|(_, entry)| entry.window.is_none())
            .map(|(&pane, _)| pane)
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
            self.owners.remove(&window);
        }
    }

    /// Replace the target a pane matches against once it next waits.
    pub fn reselect(&mut self, pane: PaneId, target: Target) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.target = target;
        }
    }

    /// Whether a pane may own a window: nobody else owns it and the pane owns
    /// no other window.
    pub fn claimable(&self, pane: PaneId, window: WindowId) -> bool {
        self.panes.get(&pane).is_some_and(|entry| {
            entry.window.is_none_or(|owned| owned == window)
                && self.owners.get(&window).is_none_or(|&owner| owner == pane)
        })
    }

    /// Make a waiting pane own a window, returning the activation token it
    /// was waiting for. Does nothing unless the window is
    /// [claimable](Self::claimable) by the pane.
    pub fn show(&mut self, pane: PaneId, window: WindowId) -> Option<String> {
        if !self.claimable(pane, window) {
            return None;
        }
        let entry = self.panes.get_mut(&pane)?;
        let previous = entry.window.replace(window);
        self.owners.insert(window, pane);
        match (&entry.target, previous) {
            (Target::Activation(token), None) => Some(token.clone()),
            _ => None,
        }
    }

    /// Release a pane's window, leaving the pane attached and waiting on its
    /// target. Returns the released window.
    pub fn unbind(&mut self, pane: PaneId) -> Option<WindowId> {
        let window = self.panes.get_mut(&pane)?.window.take()?;
        self.owners.remove(&window);
        Some(window)
    }

    /// Detach a pane and return its state.
    pub fn remove(&mut self, pane: PaneId) -> Option<AttachedPane<P>> {
        let removed = self.panes.remove(&pane)?;
        if let Some(window) = removed.window {
            self.owners.remove(&window);
        }
        Some(removed)
    }

    /// Pane owning a window.
    pub fn owner(&self, window: WindowId) -> Option<PaneId> {
        self.owners.get(&window).copied()
    }

    /// Windows owned by attached panes, in pane-identity order.
    pub fn windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.panes.values().filter_map(AttachedPane::window)
    }
}
