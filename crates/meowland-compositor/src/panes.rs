//! Pane attachment, window rebinding, and presentation ownership.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, FrameAck, FrameTransport, Target};
use smithay::utils::{Logical, Size};

use super::{
    AttachedPane, Compositor, Matcher, PaneId, Selection, ServerToPane, WindowId, frame::PaneState,
    input,
};

impl Compositor {
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
        if self.input.keyboard_state.owner == Some(pane) {
            input::reset_keyboard(self);
        }
        let removed = self.attachments.remove(pane);
        let window = removed.as_ref().and_then(AttachedPane::window);
        self.clipboard.detach(pane);
        if let Some(Selection::Activation(token)) = removed.as_ref().and_then(AttachedPane::waiting)
        {
            self.forget_activation(token);
        }
        if self.input.last_input_pane == Some(pane) {
            self.input.last_input_pane = None;
        }
        if self
            .input
            .cursor
            .sent
            .as_ref()
            .is_some_and(|(sent, _)| *sent == pane)
        {
            self.input.cursor.sent = None;
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
        if self.windows.catalog.focused() == Some(window)
            && !self.attachments.visible(window)
            && let Some(next) = self.attachments.first_window()
        {
            self.focus_window(next);
        }
    }

    pub(crate) fn resize_pane(&mut self, pane: PaneId, size: FrameSize) {
        if let Some(window) = self.attachments.resize(pane, size) {
            self.apply_window_state(window);
        }
    }

    pub(crate) fn focus_pane(
        &mut self,
        pane: PaneId,
        target: Target,
        follow: Follow,
        size: FrameSize,
    ) {
        let selection = match target {
            Target::Activation(token) if !self.protocols.launches.contains_key(&token) => {
                self.emit(pane, ServerToPane::Release("no such activation".into()));
                return;
            }
            Target::Activation(token) => Selection::Activation(token),
            Target::Selector(selector) => match Matcher::new(selector) {
                Ok(matcher) => Selection::Matching(matcher),
                Err(error) => {
                    self.emit(
                        pane,
                        ServerToPane::Release(format!("invalid selector: {error}")),
                    );
                    return;
                }
            },
        };
        let window = self.selected_window(&selection);
        if window.is_none() && follow == Follow::Current {
            self.emit(pane, ServerToPane::Release("no such window".into()));
            return;
        }
        self.attachments.insert(pane, selection, follow, size);
        if let Some(window) = window {
            self.show_pane_window(pane, window);
        }
    }

    fn show_pane_window(&mut self, pane: PaneId, window: WindowId) {
        self.bind_pane_window(pane, window);
        self.focus_window(window);
    }

    /// Show a window in a pane. A pane following every match of a launch
    /// keeps matching the activated window's app ID afterwards.
    pub(crate) fn bind_pane_window(&mut self, pane: PaneId, window: WindowId) {
        let Some((previous, follow)) = self
            .attachments
            .get(pane)
            .map(|entry| (entry.window(), entry.follow))
        else {
            return;
        };
        if let Some(token) = self.attachments.show(pane, window) {
            self.forget_activation(&token);
            if follow == Follow::Every {
                let app_id = self.windows.catalog.app_id(window).to_owned();
                self.attachments
                    .reselect(pane, Selection::Matching(Matcher::ExactAppId(app_id)));
            }
        }
        if previous != Some(window) {
            if self.input.keyboard_state.owner == Some(pane) {
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
                self.selected_window(entry.waiting()?)
                    .map(|window| (pane, window))
            })
            .collect();
        for (pane, window) in ready {
            self.show_pane_window(pane, window);
        }
    }

    /// Return panes following every match of a window that went away to
    /// waiting, then rebind them to other matching windows.
    pub(crate) fn unbind_following_panes(&mut self, window: WindowId) {
        for pane in self.panes_showing(window) {
            if self
                .attachments
                .get(pane)
                .is_none_or(|entry| entry.follow != Follow::Every)
            {
                continue;
            }
            self.attachments.unbind(pane);
            self.emit(pane, ServerToPane::Title(String::new()));
            self.send_pane_cursor(pane);
        }
        self.resolve_pending_panes();
    }
}

impl meowland_panes::Presentation for PaneState {
    fn new(size: FrameSize) -> Self {
        Self::new(size)
    }

    fn size(&self) -> FrameSize {
        self.size
    }

    fn set_size(&mut self, size: FrameSize) {
        self.size = size;
    }

    fn set_transport(&mut self, transport: FrameTransport) {
        self.transport = transport;
    }

    fn due(&self) -> bool {
        Self::due(self)
    }

    fn mark_dirty(&mut self) {
        Self::mark_dirty(self);
    }

    fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        Self::ack(self, pane, ack);
    }
}
