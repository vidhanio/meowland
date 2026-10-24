//! Pane attachment, window rebinding, and presentation ownership.

use meowland_core::pixels::FrameSize;
use meowland_protocol::Show;
use smithay::utils::{Logical, Size};

use super::{Binding, Event, Pane, PaneId, Selection, State, WindowId, input};

impl State {
    /// The last-used pane decides the window's size; otherwise use the first
    /// attached pane, then the output mode.
    pub fn pane_size(&self, window: WindowId) -> Size<i32, Logical> {
        self.attachments.size(window).map_or(self.mode, |size| {
            self.logical_size(size.width(), size.height())
        })
    }

    pub fn panes_showing(&self, window: WindowId) -> Vec<PaneId> {
        self.attachments.showing(window)
    }

    /// The lowest-numbered pane sets the output mode seen by clients.
    fn insert_pane(&mut self, pane: PaneId, entry: Pane) {
        let size = entry.presentation.size;
        self.attachments.panes.insert(pane, entry);
        if self.attachments.first() == Some(pane) {
            self.configure_output(size);
        }
    }

    /// Remove every pane-keyed entry, restoring the remaining output mode and
    /// window size owner.
    pub fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        if self.keyboard_pane == Some(pane) {
            input::reset_keyboard(self);
        }
        let first = self.attachments.first() == Some(pane);
        let removed = self.attachments.remove(pane);
        let window = removed.as_ref().and_then(Pane::window);
        self.clipboard.detach(pane);
        if let Some(Pane {
            binding: Binding::Waiting(Selection::Activation(token)),
            ..
        }) = removed
        {
            self.forget_activation(&token);
        }
        if self.cursor_pane == Some(pane) {
            self.cursor_pane = None;
        }
        if self
            .sent_cursor
            .as_ref()
            .is_some_and(|(sent, _)| *sent == pane)
        {
            self.sent_cursor = None;
        }
        if first && let Some(next) = self.attachments.first() {
            self.configure_output(self.attachments.panes[&next].presentation.size);
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
    pub fn release_pane_window(&mut self, pane: PaneId) {
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

    pub fn follow_panes(&mut self, window: WindowId) {
        for pane in self.attachments.following() {
            self.bind_pane_window(pane, window);
        }
    }

    pub fn resize_pane(&mut self, pane: PaneId, size: FrameSize) {
        let Some(entry) = self.attachments.panes.get_mut(&pane) else {
            return;
        };
        entry.presentation.size = size;
        entry.presentation.mark_dirty();
        let window = entry.window();
        if self.attachments.first() == Some(pane) {
            self.configure_output(size);
        }
        if let Some(window) = window {
            self.apply_window_state(window);
        }
    }

    pub fn attach_pane(&mut self, pane: PaneId, show: Show, size: FrameSize) {
        if matches!(&show, Show::Activation(token) if !self.launches.contains_key(token)) {
            let _ = self.events.send(Event::Release {
                pane,
                reason: "no such activation".into(),
            });
            return;
        }
        let window = self.selected_window(&show);
        if window.is_none() && matches!(show, Show::Id(_)) {
            let _ = self.events.send(Event::Release {
                pane,
                reason: "no such window".into(),
            });
            return;
        }
        let token = match &show {
            Show::Activation(token) if window.is_some() => Some(token.clone()),
            _ => None,
        };
        self.insert_pane(pane, Pane::new(show, window, size));
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

    fn bind_pane_window(&mut self, pane: PaneId, window: WindowId) {
        let Some(entry) = self.attachments.panes.get_mut(&pane) else {
            return;
        };
        let previous = entry.window();
        let token = entry.show(window);
        if let Some(token) = token {
            self.forget_activation(&token);
        }
        if previous != Some(window) {
            if self.keyboard_pane == Some(pane) {
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

    pub fn resolve_pending_panes(&mut self) {
        let ready: Vec<_> = self
            .attachments
            .panes
            .iter()
            .filter_map(|(pane, entry)| {
                let show = match &entry.binding {
                    Binding::Waiting(Selection::AppId(id)) => Show::AppId(id.clone()),
                    Binding::Waiting(Selection::Activation(token)) => {
                        Show::Activation(token.clone())
                    }
                    Binding::Empty | Binding::Showing(_) => return None,
                };
                self.selected_window(&show).map(|window| (*pane, window))
            })
            .collect();
        for (pane, window) in ready {
            self.show_pane_window(pane, window);
        }
    }
}
