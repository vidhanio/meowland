//! Pane bindings and selection policy, independent of Wayland resources.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameAck, FrameTransport, Show, clipboard::Transport};

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
enum Binding {
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
    binding: Binding,
    pub policy: Policy,
    pub presentation: PaneState,
    /// Clipboard transport the pane's terminal reported after probing.
    ///
    /// Exports assume full MIME support until a pane reports otherwise, so a
    /// selection change before the first frame still reaches the terminal.
    pub clipboard: Transport,
}

impl AttachedPane {
    fn new(show: Show, window: Option<WindowId>, size: FrameSize) -> Self {
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
            clipboard: Transport::Kitty,
        }
    }

    pub(crate) const fn window(&self) -> Option<WindowId> {
        match self.binding {
            Binding::Showing(window) => Some(window),
            Binding::Waiting(_) | Binding::Empty => None,
        }
    }

    /// Selection this pane cannot resolve to a window yet, if any.
    pub(crate) const fn waiting(&self) -> Option<&Selection> {
        match &self.binding {
            Binding::Waiting(selection) => Some(selection),
            Binding::Empty | Binding::Showing(_) => None,
        }
    }

    fn show(&mut self, window: WindowId) -> Option<String> {
        let previous = std::mem::replace(&mut self.binding, Binding::Showing(window));
        self.presentation.mark_dirty();
        match previous {
            Binding::Waiting(Selection::Activation(token)) => Some(token),
            _ => None,
        }
    }

    fn clear(&mut self) {
        self.binding = Binding::Empty;
        self.presentation.mark_dirty();
    }
}

/// Attached panes plus the index from windows to the panes showing them.
///
/// Binding changes go through this type so the index cannot drift.
#[derive(Default)]
pub(crate) struct Attachments {
    panes: BTreeMap<PaneId, AttachedPane>,
    by_window: HashMap<WindowId, BTreeSet<PaneId>>,
    deciding: HashMap<WindowId, PaneId>,
    due: usize,
}

impl Attachments {
    pub(crate) fn len(&self) -> usize {
        self.panes.len()
    }

    pub(crate) fn contains(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
    }

    pub(crate) fn get(&self, pane: PaneId) -> Option<&AttachedPane> {
        self.panes.get(&pane)
    }

    /// Number of panes whose frame is dirty and waiting for dispatch.
    pub(crate) const fn due_frames(&self) -> usize {
        self.due
    }

    /// Reset after every dirty frame was handed to
    /// [`super::frame::PaneState::frame`].
    pub(crate) const fn clear_due(&mut self) {
        self.due = 0;
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&PaneId, &AttachedPane)> {
        self.panes.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&PaneId, &mut AttachedPane)> {
        self.panes.iter_mut()
    }

    /// Attach a pane, replacing any previous attachment with the same ID.
    pub(crate) fn insert(
        &mut self,
        pane: PaneId,
        show: Show,
        window: Option<WindowId>,
        size: FrameSize,
    ) {
        let entry = AttachedPane::new(show, window, size);
        let window = entry.window();
        if let Some(previous) = self.panes.insert(pane, entry) {
            if previous.presentation.due() {
                self.due -= 1;
            }
            if let Some(previous) = previous.window() {
                self.unlink(previous, pane);
            }
        }
        if let Some(window) = window {
            self.link(window, pane);
        }
    }

    /// Bind a pane to a window, returning the activation token it was waiting
    /// for.
    pub(crate) fn show(&mut self, pane: PaneId, window: WindowId) -> Option<String> {
        let entry = self.panes.get_mut(&pane)?;
        let was = entry.presentation.due();
        let previous = entry.window();
        let token = entry.show(window);
        track_due(&mut self.due, was, entry.presentation.due());
        if previous != Some(window) {
            if let Some(previous) = previous {
                self.unlink(previous, pane);
            }
            self.link(window, pane);
        }
        token
    }

    /// Leave a pane attached but showing no window.
    pub(crate) fn clear(&mut self, pane: PaneId) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let was = entry.presentation.due();
        let previous = entry.window();
        entry.clear();
        track_due(&mut self.due, was, entry.presentation.due());
        if let Some(previous) = previous {
            self.unlink(previous, pane);
        }
    }

    pub(crate) fn remove(&mut self, pane: PaneId) -> Option<AttachedPane> {
        self.deciding.retain(|_, owner| *owner != pane);
        let removed = self.panes.remove(&pane)?;
        if removed.presentation.due() {
            self.due -= 1;
        }
        if let Some(window) = removed.window() {
            self.unlink(window, pane);
        }
        Some(removed)
    }

    pub(crate) fn showing(&self, window: WindowId) -> Vec<PaneId> {
        self.by_window
            .get(&window)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    pub(crate) fn first_window(&self) -> Option<WindowId> {
        self.panes.values().find_map(AttachedPane::window)
    }

    pub(crate) fn following(&self) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter_map(|(&pane, entry)| (entry.policy == Policy::FollowNewest).then_some(pane))
            .collect()
    }

    pub(crate) fn decide(&mut self, window: WindowId, pane: PaneId) -> bool {
        self.deciding.insert(window, pane) != Some(pane)
    }

    pub(crate) fn owner(&self, window: WindowId) -> Option<PaneId> {
        self.deciding
            .get(&window)
            .copied()
            .filter(|pane| {
                self.panes
                    .get(pane)
                    .is_some_and(|entry| entry.window() == Some(window))
            })
            .or_else(|| self.first_showing(window))
    }

    pub(crate) fn first_showing(&self, window: WindowId) -> Option<PaneId> {
        self.by_window
            .get(&window)
            .and_then(|panes| panes.first().copied())
    }

    pub(crate) fn size(&self, window: WindowId) -> Option<FrameSize> {
        self.owner(window)
            .and_then(|id| self.panes.get(&id))
            .map(|pane| pane.presentation.size)
    }

    pub(crate) fn visible(&self, window: WindowId) -> bool {
        self.by_window.contains_key(&window)
    }

    pub(crate) fn for_each_showing(
        &self,
        window: WindowId,
        mut visit: impl FnMut(PaneId, &AttachedPane),
    ) {
        for &pane in self.by_window.get(&window).into_iter().flatten() {
            if let Some(entry) = self.panes.get(&pane) {
                visit(pane, entry);
            }
        }
    }

    pub(crate) fn for_each_showing_mut(
        &mut self,
        window: WindowId,
        mut visit: impl FnMut(PaneId, &mut AttachedPane),
    ) {
        for &pane in self.by_window.get(&window).into_iter().flatten() {
            if let Some(entry) = self.panes.get_mut(&pane) {
                let was = entry.presentation.due();
                visit(pane, entry);
                track_due(&mut self.due, was, entry.presentation.due());
            }
        }
    }

    pub(crate) fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let was = entry.presentation.due();
        entry.presentation.ack(pane, ack);
        track_due(&mut self.due, was, entry.presentation.due());
    }

    pub(crate) fn resize(&mut self, pane: PaneId, size: FrameSize) -> Option<WindowId> {
        let entry = self.panes.get_mut(&pane)?;
        if entry.presentation.size == size {
            return None;
        }
        let was = entry.presentation.due();
        entry.presentation.size = size;
        entry.presentation.mark_dirty();
        let now = entry.presentation.due();
        let window = entry.window();
        track_due(&mut self.due, was, now);
        window
    }

    pub(crate) fn set_transport(&mut self, pane: PaneId, transport: FrameTransport) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.presentation.transport = transport;
        }
    }

    pub(crate) fn set_clipboard_transport(&mut self, pane: PaneId, transport: Transport) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.clipboard = transport;
        }
    }

    pub(crate) fn forget_window(&mut self, window: WindowId) {
        self.deciding.remove(&window);
    }

    fn link(&mut self, window: WindowId, pane: PaneId) {
        self.by_window.entry(window).or_default().insert(pane);
    }

    fn unlink(&mut self, window: WindowId, pane: PaneId) {
        if let Some(panes) = self.by_window.get_mut(&window) {
            panes.remove(&pane);
            if panes.is_empty() {
                self.by_window.remove(&window);
            }
        }
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the flags are the pane's before and after dirty state"
)]
const fn track_due(due: &mut usize, was: bool, now: bool) {
    match (was, now) {
        (true, false) => *due -= 1,
        (false, true) => *due += 1,
        _ => {}
    }
}
