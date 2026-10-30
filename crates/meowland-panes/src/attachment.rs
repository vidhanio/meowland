//! Pane bindings and selection policy, independent of Wayland resources.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, FrameAck, FrameTransport, clipboard::Transport};

use crate::{Selection, WindowId};

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
    selection: Selection,
    window: Option<WindowId>,
    /// How long the pane keeps matching its selection.
    pub follow: Follow,
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
        self.window
    }

    /// Selection identifying the windows this pane may show.
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Selection this pane is waiting to resolve while showing no window.
    pub const fn waiting(&self) -> Option<&Selection> {
        match self.window {
            None => Some(&self.selection),
            Some(_) => None,
        }
    }
}

impl<P> fmt::Debug for AttachedPane<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachedPane")
            .field("selection", &self.selection)
            .field("window", &self.window)
            .field("follow", &self.follow)
            .field("clipboard", &self.clipboard)
            .finish_non_exhaustive()
    }
}

impl<P: Presentation> AttachedPane<P> {
    fn new(selection: Selection, follow: Follow, size: FrameSize) -> Self {
        Self {
            selection,
            window: None,
            follow,
            presentation: P::new(size),
            clipboard: Transport::Kitty,
        }
    }

    fn show(&mut self, window: WindowId) -> Option<String> {
        let previous = self.window.replace(window);
        self.presentation.mark_dirty();
        match (&self.selection, previous) {
            (Selection::Activation(token), None) => Some(token.clone()),
            _ => None,
        }
    }

    fn unbind(&mut self) {
        self.window = None;
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

    /// Iterate over attached panes in pane-identity order.
    pub fn iter(&self) -> impl Iterator<Item = (&PaneId, &AttachedPane<P>)> {
        self.panes.iter()
    }

    /// Attach a pane waiting for its selection, replacing any previous
    /// attachment with the same ID.
    pub fn insert(&mut self, pane: PaneId, selection: Selection, follow: Follow, size: FrameSize) {
        let entry = AttachedPane::<P>::new(selection, follow, size);
        let new_due = entry.presentation.due();
        if let Some(previous) = self.panes.insert(pane, entry) {
            track_due(&mut self.due, previous.presentation.due(), new_due);
            if let Some(previous) = previous.window() {
                self.unlink(previous, pane);
            }
        } else if new_due {
            self.due += 1;
        }
    }

    /// Replace the selection a pane matches against once it next waits.
    pub fn reselect(&mut self, pane: PaneId, selection: Selection) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.selection = selection;
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

    /// Leave a pane attached but showing no window, waiting on its selection.
    pub fn unbind(&mut self, pane: PaneId) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let was = entry.presentation.due();
        let previous = entry.window();
        entry.unbind();
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
