//! Pane bindings and selection policy, independent of Wayland resources.

use std::collections::HashMap;

use meowland_core::pixels::FrameSize;
use meowland_protocol::Show;

use super::frame::PaneState;

/// Host-assigned identity of a terminal pane, distinct from a Wayland window
/// ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u64);

/// Compositor-assigned identity stable for one Wayland toplevel's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u64);

#[derive(Clone, Debug)]
pub(crate) enum Selection {
    AppId(String),
    Activation(String),
}

#[derive(Clone, Debug)]
pub(crate) enum Binding {
    Waiting(Selection),
    Empty,
    Showing(WindowId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    Fixed,
    FollowNewest,
}

pub(crate) struct AttachedPane {
    pub binding: Binding,
    pub policy: Policy,
    pub presentation: PaneState,
}

impl AttachedPane {
    pub(crate) fn new(show: Show, window: Option<WindowId>, size: FrameSize) -> Self {
        let policy = if matches!(show, Show::Newest)
            || (window.is_none() && matches!(show, Show::Focused))
        {
            Policy::FollowNewest
        } else {
            Policy::Fixed
        };
        let binding = window.map_or_else(
            || match show {
                Show::AppId(id) => Binding::Waiting(Selection::AppId(id)),
                Show::Activation(token) => Binding::Waiting(Selection::Activation(token)),
                Show::Id(_) | Show::Newest | Show::Focused => Binding::Empty,
            },
            Binding::Showing,
        );
        Self {
            binding,
            policy,
            presentation: PaneState::new(size),
        }
    }

    pub(crate) const fn window(&self) -> Option<WindowId> {
        match self.binding {
            Binding::Showing(window) => Some(window),
            Binding::Waiting(_) | Binding::Empty => None,
        }
    }

    pub(crate) fn show(&mut self, window: WindowId) -> Option<String> {
        let previous = std::mem::replace(&mut self.binding, Binding::Showing(window));
        self.presentation.mark_dirty();
        match previous {
            Binding::Waiting(Selection::Activation(token)) => Some(token),
            _ => None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.binding = Binding::Empty;
        self.presentation.mark_dirty();
    }
}

#[derive(Default)]
pub(crate) struct Attachments {
    pub panes: HashMap<PaneId, AttachedPane>,
    deciding: HashMap<WindowId, PaneId>,
}

impl Attachments {
    pub(crate) fn first(&self) -> Option<PaneId> {
        self.panes.keys().copied().min()
    }

    pub(crate) fn first_window(&self) -> Option<WindowId> {
        self.panes
            .iter()
            .filter_map(|(id, pane)| pane.window().map(|window| (*id, window)))
            .min_by_key(|(id, _)| *id)
            .map(|(_, window)| window)
    }

    pub(crate) fn showing(&self, window: WindowId) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter_map(|(id, pane)| (pane.window() == Some(window)).then_some(*id))
            .collect()
    }

    pub(crate) fn following(&self) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter_map(|(id, pane)| (pane.policy == Policy::FollowNewest).then_some(*id))
            .collect()
    }

    pub(crate) fn decide(&mut self, window: WindowId, pane: PaneId) -> bool {
        self.deciding.insert(window, pane) != Some(pane)
    }

    pub(crate) fn owner(&self, window: WindowId) -> Option<PaneId> {
        self.deciding
            .get(&window)
            .copied()
            .filter(|id| {
                self.panes
                    .get(id)
                    .is_some_and(|pane| pane.window() == Some(window))
            })
            .or_else(|| {
                self.panes
                    .iter()
                    .filter(|(_, pane)| pane.window() == Some(window))
                    .min_by_key(|(id, _)| *id)
                    .map(|(id, _)| *id)
            })
    }

    pub(crate) fn size(&self, window: WindowId) -> Option<FrameSize> {
        self.owner(window)
            .and_then(|id| self.panes.get(&id))
            .map(|pane| pane.presentation.size)
    }

    pub(crate) fn visible(&self, window: WindowId) -> bool {
        self.panes
            .values()
            .any(|pane| pane.window() == Some(window))
    }

    pub(crate) fn remove(&mut self, pane: PaneId) -> Option<AttachedPane> {
        self.deciding.retain(|_, owner| *owner != pane);
        self.panes.remove(&pane)
    }

    pub(crate) fn forget_window(&mut self, window: WindowId) {
        self.deciding.remove(&window);
    }
}
