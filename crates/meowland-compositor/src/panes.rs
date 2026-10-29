//! Pane attachment, window rebinding, and presentation ownership.

use meowland_core::pixels::FrameSize;
use meowland_protocol::Show;
use smithay::utils::{Logical, Size};

use super::{AttachedPane, PaneId, Selection, ServerToPane, State, WindowId, input};

impl State {
    /// The last-used pane decides the window's size; otherwise use the first
    /// attached pane, then the output mode.
    pub(crate) fn pane_size(&self, window: WindowId) -> Size<i32, Logical> {
        self.attachments.size(window).map_or(self.mode, |size| {
            self.logical_size(size.width(), size.height())
        })
    }

    pub(crate) fn panes_showing(&self, window: WindowId) -> Vec<PaneId> {
        self.attachments.showing(window)
    }

    /// Remove every pane-keyed entry and restore the remaining window size
    /// owner.
    pub(crate) fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        if self.keyboard_state.owner == Some(pane) {
            input::reset_keyboard(self);
        }
        let removed = self.attachments.remove(pane);
        let window = removed.as_ref().and_then(AttachedPane::window);
        self.clipboard.detach(pane);
        if let Some(token) =
            removed
                .as_ref()
                .and_then(AttachedPane::waiting)
                .and_then(|selection| match selection {
                    Selection::Activation(token) => Some(token.clone()),
                    Selection::AppId(_) => None,
                })
        {
            self.forget_activation(&token);
        }
        if self.last_input_pane == Some(pane) {
            self.last_input_pane = None;
        }
        if self
            .cursor
            .sent
            .as_ref()
            .is_some_and(|(sent, _)| *sent == pane)
        {
            self.cursor.sent = None;
        }
        if let Some(window) = window {
            self.leave_output(window);
            if self.attachments.visible(window) {
                self.apply_window_state(window);
            }
        }
        window
    }

    /// Move focus to another attached window when the released window is no
    /// longer visible. Empty panes do not block focus restoration.
    pub(crate) fn release_pane_window(&mut self, pane: PaneId) {
        let Some(window) = self.remove_pane(pane) else {
            return;
        };
        if self.catalog.focused() == Some(window)
            && !self.attachments.visible(window)
            && let Some(next) = self.attachments.first_window()
        {
            self.focus_window(next);
        }
    }

    pub(crate) fn follow_panes(&mut self, window: WindowId) {
        for pane in self.attachments.following() {
            self.bind_pane_window(pane, window);
        }
    }

    pub(crate) fn resize_pane(&mut self, pane: PaneId, size: FrameSize) {
        let Some(entry) = self.attachments.get_mut(pane) else {
            return;
        };
        if entry.presentation.size == size {
            return;
        }
        entry.presentation.size = size;
        entry.presentation.mark_dirty();
        if let Some(window) = entry.window() {
            self.apply_window_state(window);
        }
    }

    pub(crate) fn attach_pane(&mut self, pane: PaneId, show: Show, size: FrameSize) {
        if matches!(&show, Show::Activation(token) if !self.launches.contains_key(token)) {
            self.emit(pane, ServerToPane::Release("no such activation".into()));
            return;
        }
        let window = self.selected_window(&show);
        if window.is_none() && matches!(show, Show::Id(_)) {
            self.emit(pane, ServerToPane::Release("no such window".into()));
            return;
        }
        let token = match &show {
            Show::Activation(token) if window.is_some() => Some(token.clone()),
            _ => None,
        };
        self.attachments.insert(pane, show, window, size);
        if let Some(window) = window {
            self.show_pane_window(pane, window);
        }
        if let Some(token) = token {
            self.forget_activation(&token);
        }
    }

    fn show_pane_window(&mut self, pane: PaneId, window: WindowId) {
        self.bind_pane_window(pane, window);
        self.focus_window(window);
    }

    pub(crate) fn bind_pane_window(&mut self, pane: PaneId, window: WindowId) {
        let Some(previous) = self.attachments.get(pane).map(AttachedPane::window) else {
            return;
        };
        let token = self.attachments.show(pane, window);
        if let Some(token) = token {
            self.forget_activation(&token);
        }
        if previous != Some(window) {
            if self.keyboard_state.owner == Some(pane) {
                input::reset_keyboard(self);
            }
            if let Some(previous) = previous {
                self.leave_output(previous);
                if self.attachments.visible(previous) {
                    self.apply_window_state(previous);
                }
            }
        }
        self.resume_callbacks(window);
        self.enter_output(window);
        self.send_title(pane, window);
        self.send_pane_cursor(pane);
        self.apply_window_state(window);
    }

    pub(crate) fn resolve_pending_panes(&mut self) {
        let ready: Vec<_> = self
            .attachments
            .iter()
            .filter_map(|(&pane, entry)| {
                let show = match entry.waiting()? {
                    Selection::AppId(id) => Show::AppId(id.clone()),
                    Selection::Activation(token) => Show::Activation(token.clone()),
                };
                self.selected_window(&show).map(|window| (pane, window))
            })
            .collect();
        for (pane, window) in ready {
            self.show_pane_window(pane, window);
        }
    }
}
