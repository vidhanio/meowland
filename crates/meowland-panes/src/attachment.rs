//! Pane bindings and selection policy, independent of Wayland resources.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameAck, FrameTransport, Show, WindowId, clipboard::Transport};

/// Host-assigned identity of a terminal pane, distinct from a Wayland window
/// ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(u64);

impl PaneId {
    /// Numeric identity as assigned by the server.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for PaneId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// How a pane that is waiting for a window identifies it.
#[derive(Clone, Debug)]
pub enum Selection {
    /// Match the newest window with this application ID.
    AppId(String),
    /// Match the window claimed by this activation token.
    Activation(String),
}

#[derive(Clone, Debug)]
enum Binding {
    Waiting(Selection),
    Empty,
    Showing(WindowId),
}

/// Whether a pane stays on its window or follows the newest window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Keep showing the bound window.
    Fixed,
    /// Rebind to the newest window when the current one disappears.
    FollowNewest,
}

/// Frame pacing state a pane needs from the compositor.
///
/// The compositor supplies the concrete presentation type but delegates pane
/// bookkeeping to [`Attachments`], which only needs these operations.
///
/// [`Attachments`] re-reads [`due`](Presentation::due) after every mutation it
/// performs, including [`set_size`](Presentation::set_size) and
/// [`set_transport`](Presentation::set_transport), so implementations may
/// change their dirty state in those methods without desynchronizing the
/// dirty-frame count.
pub trait Presentation {
    /// Create the presentation for a new pane at the given size.
    fn new(size: FrameSize) -> Self;
    /// Pixel size the pane currently presents.
    fn size(&self) -> FrameSize;
    /// Record a new pane size.
    fn set_size(&mut self, size: FrameSize);
    /// Record the transport the pane asked to receive frames on.
    fn set_transport(&mut self, transport: FrameTransport);
    /// Whether a frame is dirty and waiting for dispatch.
    fn due(&self) -> bool;
    /// Mark the currently presented content as stale.
    fn mark_dirty(&mut self);
    /// Apply the pane's frame acknowledgement.
    fn ack(&mut self, pane: PaneId, ack: FrameAck);
}

/// A pane attached to a window selector.
pub struct AttachedPane<P> {
    binding: Binding,
    /// Whether the pane follows the newest window.
    pub policy: Policy,
    /// Frame pacing state for the pane.
    pub presentation: P,
    /// Clipboard transport the pane's terminal reported after probing.
    ///
    /// Exports assume full MIME support until a pane reports otherwise, so a
    /// selection change before the first frame still reaches the terminal.
    pub clipboard: Transport,
}

impl<P> AttachedPane<P> {
    /// Window the pane currently shows, if any.
    pub const fn window(&self) -> Option<WindowId> {
        match self.binding {
            Binding::Showing(window) => Some(window),
            Binding::Waiting(_) | Binding::Empty => None,
        }
    }

    /// Selection this pane cannot resolve to a window yet, if any.
    pub const fn waiting(&self) -> Option<&Selection> {
        match &self.binding {
            Binding::Waiting(selection) => Some(selection),
            Binding::Empty | Binding::Showing(_) => None,
        }
    }
}

impl<P> fmt::Debug for AttachedPane<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachedPane")
            .field("binding", &self.binding)
            .field("policy", &self.policy)
            .field("clipboard", &self.clipboard)
            .finish_non_exhaustive()
    }
}

impl<P: Presentation> AttachedPane<P> {
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
            presentation: P::new(size),
            clipboard: Transport::Kitty,
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
pub struct Attachments<P> {
    panes: BTreeMap<PaneId, AttachedPane<P>>,
    by_window: HashMap<WindowId, BTreeSet<PaneId>>,
    deciding: HashMap<WindowId, PaneId>,
    due: usize,
}

impl<P> Default for Attachments<P> {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
            by_window: HashMap::new(),
            deciding: HashMap::new(),
            due: 0,
        }
    }
}

impl<P> fmt::Debug for Attachments<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Attachments")
            .field("panes", &self.panes.len())
            .field("windows", &self.by_window.len())
            .field("due", &self.due)
            .finish_non_exhaustive()
    }
}

impl<P: Presentation> Attachments<P> {
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

    /// Number of panes whose frame is dirty and waiting for dispatch.
    pub const fn due_frames(&self) -> usize {
        self.due
    }

    /// Reset after every dirty frame was handed to the compositor.
    pub const fn clear_due(&mut self) {
        self.due = 0;
    }

    /// Iterate over attached panes in pane-identity order.
    pub fn iter(&self) -> impl Iterator<Item = (&PaneId, &AttachedPane<P>)> {
        self.panes.iter()
    }

    /// Attach a pane, replacing any previous attachment with the same ID.
    pub fn insert(&mut self, pane: PaneId, show: Show, window: Option<WindowId>, size: FrameSize) {
        let entry = AttachedPane::<P>::new(show, window, size);
        let new_due = entry.presentation.due();
        let window = entry.window();
        if let Some(previous) = self.panes.insert(pane, entry) {
            track_due(&mut self.due, previous.presentation.due(), new_due);
            if let Some(previous) = previous.window() {
                self.unlink(previous, pane);
            }
        } else if new_due {
            self.due += 1;
        }
        if let Some(window) = window {
            self.link(window, pane);
        }
    }

    /// Bind a pane to a window, returning the activation token it was waiting
    /// for.
    pub fn show(&mut self, pane: PaneId, window: WindowId) -> Option<String> {
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
    pub fn clear(&mut self, pane: PaneId) {
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

    /// Detach a pane and return its state.
    pub fn remove(&mut self, pane: PaneId) -> Option<AttachedPane<P>> {
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

    /// Panes showing a window, in pane-identity order.
    pub fn showing(&self, window: WindowId) -> Vec<PaneId> {
        self.by_window
            .get(&window)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    /// First window shown by an attached pane, in pane-identity order.
    pub fn first_window(&self) -> Option<WindowId> {
        self.panes.values().find_map(AttachedPane::window)
    }

    /// Panes that follow the newest window.
    pub fn following(&self) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter_map(|(&pane, entry)| (entry.policy == Policy::FollowNewest).then_some(pane))
            .collect()
    }

    /// Record the pane that most recently routed input to a window.
    ///
    /// Returns whether the pane changed.
    pub fn decide(&mut self, window: WindowId, pane: PaneId) -> bool {
        self.deciding.insert(window, pane) != Some(pane)
    }

    /// Pane whose size and input the window follows.
    pub fn owner(&self, window: WindowId) -> Option<PaneId> {
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

    /// First pane showing a window, in pane-identity order.
    pub fn first_showing(&self, window: WindowId) -> Option<PaneId> {
        self.by_window
            .get(&window)
            .and_then(|panes| panes.first().copied())
    }

    /// Size requested by the window's owning pane.
    pub fn size(&self, window: WindowId) -> Option<FrameSize> {
        self.owner(window)
            .and_then(|id| self.panes.get(&id))
            .map(|pane| pane.presentation.size())
    }

    /// Whether any pane is showing the window.
    pub fn visible(&self, window: WindowId) -> bool {
        self.by_window.contains_key(&window)
    }

    /// Visit every pane showing a window.
    pub fn for_each_showing(
        &self,
        window: WindowId,
        mut visit: impl FnMut(PaneId, &AttachedPane<P>),
    ) {
        for &pane in self.by_window.get(&window).into_iter().flatten() {
            if let Some(entry) = self.panes.get(&pane) {
                visit(pane, entry);
            }
        }
    }

    /// Visit every attached pane's presentation in pane-identity order.
    ///
    /// The callback receives the pane ID, the window it shows (if any), and
    /// only its presentation, so it cannot disturb the window index. The
    /// dirty-frame count is updated after each callback.
    pub fn for_each_presentation_mut(
        &mut self,
        mut visit: impl FnMut(PaneId, Option<WindowId>, &mut P),
    ) {
        for (&pane, entry) in &mut self.panes {
            let was = entry.presentation.due();
            visit(pane, entry.window(), &mut entry.presentation);
            track_due(&mut self.due, was, entry.presentation.due());
        }
    }

    /// Visit every pane showing a window and keep the dirty-frame count exact.
    ///
    /// The callback cannot disturb the window index; the dirty-frame count is
    /// updated after each callback.
    pub fn for_each_showing_mut(
        &mut self,
        window: WindowId,
        mut visit: impl FnMut(PaneId, &mut P),
    ) {
        for &pane in self.by_window.get(&window).into_iter().flatten() {
            if let Some(entry) = self.panes.get_mut(&pane) {
                let was = entry.presentation.due();
                visit(pane, &mut entry.presentation);
                track_due(&mut self.due, was, entry.presentation.due());
            }
        }
    }

    /// Apply a pane's frame acknowledgement.
    pub fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let was = entry.presentation.due();
        entry.presentation.ack(pane, ack);
        track_due(&mut self.due, was, entry.presentation.due());
    }

    /// Resize a pane and return the window whose state must be reapplied.
    pub fn resize(&mut self, pane: PaneId, size: FrameSize) -> Option<WindowId> {
        let entry = self.panes.get_mut(&pane)?;
        if entry.presentation.size() == size {
            return None;
        }
        let was = entry.presentation.due();
        entry.presentation.set_size(size);
        entry.presentation.mark_dirty();
        let now = entry.presentation.due();
        let window = entry.window();
        track_due(&mut self.due, was, now);
        window
    }

    /// Record the transport a pane wants frames on.
    pub fn set_transport(&mut self, pane: PaneId, transport: FrameTransport) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let was = entry.presentation.due();
        entry.presentation.set_transport(transport);
        track_due(&mut self.due, was, entry.presentation.due());
    }

    /// Record the clipboard transport a pane's terminal supports.
    pub fn set_clipboard_transport(&mut self, pane: PaneId, transport: Transport) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.clipboard = transport;
        }
    }

    /// Forget which pane last routed input to a removed window.
    pub fn forget_window(&mut self, window: WindowId) {
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
