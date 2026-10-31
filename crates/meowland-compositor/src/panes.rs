//! Pane attachment, window rebinding, and presentation ownership.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, FrameAck, FrameTransport, Selector, Target};
use smithay::utils::{Logical, Size};

use super::{
    AttachedPane, Compositor, PaneId, ServerToPane, WindowId, clipboard, frame::PaneState, input,
};

impl Compositor {
    /// The last-used pane decides the window's size; otherwise use the first
    /// attached pane, then the output mode.
    pub(crate) fn pane_size(&self, window: WindowId) -> Size<i32, Logical> {
        self.attachments.size(window).map_or(self.mode, |size| {
            self.logical_size(size.width(), size.height())
        })
    }

    /// Remove every pane-keyed entry and restore the remaining window size
    /// owner.
    pub(crate) fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        if self.input.keyboard_state.owner == Some(pane) {
            input::reset_keyboard(self);
        }
        let removed = self.attachments.remove(pane);
        let window = removed.as_ref().and_then(AttachedPane::window);
        clipboard::detach(self, pane);
        if let Some(Target::Activation(token)) = removed.as_ref().and_then(AttachedPane::waiting) {
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
        if self.windows.catalog.focused() == Some(window) && !self.attachments.visible(window) {
            let next = self
                .attachments
                .windows()
                .find(|&id| self.windows.catalog.drawable(id));
            if let Some(next) = next {
                self.focus_window(next);
            }
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
        if let Target::Activation(token) = &target
            && !self.protocols.launches.contains_key(token)
        {
            self.emit(pane, ServerToPane::Release("no such activation".into()));
            return;
        }
        let window = self.selected_window(&target);
        if window.is_none() && follow == Follow::Current {
            self.emit(pane, ServerToPane::Release("no such window".into()));
            return;
        }
        self.attachments.insert(pane, target, follow, size);
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
                    .reselect(pane, Target::Selector(Selector::AppId(app_id)));
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
    /// waiting, rebind them to other matching windows, and tell the rest to
    /// wait.
    pub(crate) fn unbind_following_panes(&mut self, window: WindowId) {
        let following: Vec<_> = self
            .attachments
            .showing(window)
            .into_iter()
            .filter(|&pane| {
                self.attachments
                    .get(pane)
                    .is_some_and(|entry| entry.follow == Follow::Every)
            })
            .collect();
        for &pane in &following {
            self.attachments.unbind(pane);
        }
        self.resolve_pending_panes();
        for pane in following {
            if self
                .attachments
                .get(pane)
                .is_some_and(|entry| entry.window().is_none())
            {
                self.emit(pane, ServerToPane::Waiting);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Fixture;

    fn messages(fixture: &mut Fixture, pane: PaneId) -> Vec<ServerToPane> {
        fixture.state.dispatch_frames();
        fixture
            .state
            .drain_events()
            .filter(|event| event.pane == pane)
            .map(|event| event.message)
            .collect()
    }

    fn frames(messages: &[ServerToPane]) -> Vec<&meowland_protocol::FrameBand> {
        messages
            .iter()
            .filter_map(|message| match message {
                ServerToPane::Frame(band) => Some(band),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn following_pane_waits_without_frames_until_a_new_match_maps() {
        let mut fixture = Fixture::new();
        let first = fixture.toplevel("app");
        fixture.present(&first.surface, 4, 4, [0, 0, 0, 255]);
        let pane = PaneId::from(1);
        fixture.state.focus_pane(
            pane,
            Target::Selector(Selector::AppId("app".into())),
            Follow::Every,
            FrameSize::new(64, 64).expect("test size"),
        );
        assert_eq!(frames(&messages(&mut fixture, pane)).len(), 1);
        fixture.state.attachments.ack(pane, FrameAck::Retained);
        fixture.present(&first.surface, 4, 4, [255, 0, 0, 255]);
        assert_eq!(frames(&messages(&mut fixture, pane)).len(), 1);

        first.surface.attach(None, 0, 0);
        first.surface.commit();
        fixture.roundtrip();
        let unmapped = messages(&mut fixture, pane);
        assert!(
            unmapped
                .iter()
                .any(|message| matches!(message, ServerToPane::Waiting)),
            "{unmapped:?}"
        );
        assert!(frames(&unmapped).is_empty(), "{unmapped:?}");
        fixture.state.attachments.ack(pane, FrameAck::Resync);
        assert!(frames(&messages(&mut fixture, pane)).is_empty());

        let second = fixture.toplevel("app");
        fixture.present(&second.surface, 4, 4, [0, 255, 0, 255]);
        let shown = messages(&mut fixture, pane);
        let shown = frames(&shown);
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].is_whole(),
            "the first frame after waiting is complete"
        );
    }

    #[test]
    fn release_skips_an_unmapped_attachment_before_a_drawable_one() {
        let mut fixture = crate::fixture::Fixture::new();
        let first = fixture.toplevel("first");
        fixture.present(&first.surface, 4, 4, [0, 0, 0, 255]);
        let second = fixture.toplevel("second");
        fixture.present(&second.surface, 4, 4, [0, 0, 0, 255]);
        let third = fixture.toplevel("third");
        fixture.present(&third.surface, 4, 4, [0, 0, 0, 255]);
        let windows = [
            fixture.window(&first),
            fixture.window(&second),
            fixture.window(&third),
        ];
        for (number, window) in (1..=3).zip(windows) {
            let pane = PaneId::from(number);
            fixture.state.attachments.insert(
                pane,
                Target::Selector(Selector::AppId(String::new())),
                Follow::Current,
                FrameSize::new(64, 64).expect("test size"),
            );
            fixture.state.attachments.show(pane, window);
        }
        first.surface.attach(None, 0, 0);
        first.surface.commit();
        fixture.roundtrip();
        fixture.state.focus_window(windows[2]);
        fixture.state.release_pane_window(PaneId::from(3));
        assert_eq!(fixture.state.windows.catalog.focused(), Some(windows[1]));
    }
}
