//! Pane attachment, window rebinding, and presentation ownership.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, Selector, Target};
use smithay::utils::{Logical, Size};

use super::{AttachedPane, Compositor, PaneId, ServerToPane, WindowId, frame::PaneState, input};

impl Compositor {
    /// The size of the window's owning pane, else the output mode.
    pub(crate) fn pane_size(&self, window: WindowId) -> Size<i32, Logical> {
        self.attachments
            .owner(window)
            .and_then(|pane| self.attachments.get(pane))
            .map_or(self.mode, |entry| {
                self.logical_size(entry.state.size.width(), entry.state.size.height())
            })
    }

    /// Mark the content of every pane showing a window as stale.
    pub(crate) fn touch(&mut self, window: WindowId) {
        for pane in self.attachments.showing(window).to_vec() {
            if let Some(state) = self.attachments.state_mut(pane) {
                state.mark_dirty();
            }
        }
    }

    /// Whether any pane has a frame waiting for dispatch.
    pub(crate) fn frames_due(&self) -> bool {
        self.attachments.iter().any(|(_, entry)| entry.state.due())
    }

    /// Remove every pane-keyed entry, returning the window the pane showed.
    pub(crate) fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        let removed = self.detach_pane(pane)?;
        self.release_waited_activation(&removed);
        removed.window()
    }

    /// [`Self::remove_pane`] without releasing the launch the pane waited
    /// for, so a replacement attachment can keep waiting for it.
    fn detach_pane(&mut self, pane: PaneId) -> Option<AttachedPane> {
        if self.input.keyboard_state.owned_by(pane) {
            input::reset_keyboard(self);
        }
        let removed = self.attachments.remove(pane);
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
        if let Some(window) = removed.as_ref().and_then(AttachedPane::window) {
            self.leave_output(window);
            if !self.attachments.showing(window).is_empty() {
                self.apply_window_state(window);
            }
        }
        removed
    }

    fn release_waited_activation(&mut self, removed: &AttachedPane) {
        if let Some(Target::Activation(token)) = removed.waiting() {
            self.release_activation(token);
        }
    }

    /// Move focus to another shown window when the released window had it.
    /// Empty panes do not block focus restoration.
    pub(crate) fn release_pane_window(&mut self, pane: PaneId) {
        let Some(window) = self.remove_pane(pane) else {
            return;
        };
        if self.windows.catalog.focused() == Some(window)
            && self.attachments.showing(window).is_empty()
        {
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
        let Some(entry) = self.attachments.get(pane) else {
            return;
        };
        if entry.state.size == size {
            return;
        }
        let window = entry.window();
        if let Some(state) = self.attachments.state_mut(pane) {
            state.size = size;
            state.mark_dirty();
        }
        if let Some(window) = window {
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
        let replaced = if self.attachments.contains(pane) {
            self.detach_pane(pane)
        } else {
            None
        };
        self.attachments
            .insert(pane, target, follow, PaneState::new(size));
        if let Some(replaced) = &replaced {
            self.release_waited_activation(replaced);
        }
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
        if let Some(state) = self.attachments.state_mut(pane) {
            state.mark_dirty();
        }
        if previous != Some(window) {
            if self.input.keyboard_state.owned_by(pane) {
                input::reset_keyboard(self);
            }
            if let Some(previous) = previous {
                self.leave_output(previous);
                if !self.attachments.showing(previous).is_empty() {
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
            .filter_map(|(pane, entry)| {
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
            .iter()
            .copied()
            .filter(|&pane| {
                self.attachments
                    .get(pane)
                    .is_some_and(|entry| entry.follow == Follow::Every)
            })
            .collect();
        for &pane in &following {
            self.attachments.unbind(pane);
            if let Some(state) = self.attachments.state_mut(pane) {
                state.mark_dirty();
            }
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

    /// Whether each frame message is a whole frame.
    fn frames(messages: &[ServerToPane]) -> Vec<bool> {
        messages
            .iter()
            .filter_map(|message| match message {
                ServerToPane::Frame(band) => Some(band.is_whole()),
                ServerToPane::SharedFrame(_) => Some(true),
                _ => None,
            })
            .collect()
    }

    fn ack(fixture: &mut Fixture, pane: PaneId) {
        if let Some(state) = fixture.state.attachments.state_mut(pane) {
            state.ack();
        }
    }

    fn shown(fixture: &Fixture, pane: PaneId) -> Option<WindowId> {
        fixture
            .state
            .attachments
            .get(pane)
            .and_then(AttachedPane::window)
    }

    #[test]
    fn panes_share_a_window_that_takes_the_size_of_its_last_input_pane() {
        let mut fixture = Fixture::new();
        let app = fixture.toplevel("app");
        fixture.present(&app.surface, 4, 4, [0, 0, 0, 255]);
        let window = fixture.window(&app);
        let target = Target::Selector(Selector::AppId("app".into()));
        let (first, second) = (PaneId::from(1), PaneId::from(2));
        let small = FrameSize::new(64, 64).expect("small pane");
        let large = FrameSize::new(128, 96).expect("large pane");
        let state = &mut fixture.state;
        state.focus_pane(first, target.clone(), Follow::Current, small);
        state.focus_pane(second, target, Follow::Current, large);
        assert_eq!(shown(&fixture, first), Some(window));
        assert_eq!(shown(&fixture, second), Some(window));
        let small_logical = fixture.state.logical_size(64, 64);
        let large_logical = fixture.state.logical_size(128, 96);
        assert_eq!(fixture.state.pane_size(window), small_logical);

        fixture.state.dispatch_frames();
        let framed: std::collections::HashSet<_> = fixture
            .state
            .drain_events()
            .filter(|event| {
                matches!(
                    event.message,
                    ServerToPane::Frame(_) | ServerToPane::SharedFrame(_)
                )
            })
            .map(|event| event.pane)
            .collect();
        assert_eq!(framed, [first, second].into());

        let pointer = meowland_protocol::Input::Pointer {
            x: 1.0,
            y: 1.0,
            button: None,
            scroll: meowland_protocol::Scroll::default(),
        };
        input::pane_input(&mut fixture.state, second, &pointer);
        assert_eq!(fixture.state.pane_size(window), large_logical);

        fixture.state.release_pane_window(second);
        assert_eq!(fixture.state.pane_size(window), small_logical);
        assert_eq!(shown(&fixture, first), Some(window));
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
        if let Some(state) = fixture.state.attachments.state_mut(pane) {
            state.shared = false;
        }
        assert_eq!(frames(&messages(&mut fixture, pane)).len(), 1);
        ack(&mut fixture, pane);
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
        assert_eq!(frames(&unmapped), [], "{unmapped:?}");
        ack(&mut fixture, pane);
        assert_eq!(frames(&messages(&mut fixture, pane)), []);

        let second = fixture.toplevel("app");
        fixture.present(&second.surface, 4, 4, [0, 255, 0, 255]);
        let shown = messages(&mut fixture, pane);
        assert_eq!(
            frames(&shown),
            [true],
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
                PaneState::new(size()),
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

    fn size() -> FrameSize {
        FrameSize::new(64, 64).expect("test size")
    }

    #[test]
    fn a_launch_outlives_one_of_two_panes_waiting_for_it() {
        let mut fixture = Fixture::new();
        let token = fixture.state.create_activation_token();
        let (first, second) = (PaneId::from(1), PaneId::from(2));
        for pane in [first, second] {
            fixture.state.focus_pane(
                pane,
                Target::Activation(token.clone()),
                Follow::First,
                size(),
            );
        }

        fixture.state.remove_pane(first);
        assert!(fixture.state.protocols.launches.contains_key(&token));
        fixture.state.remove_pane(second);
        assert!(!fixture.state.protocols.launches.contains_key(&token));
    }

    #[test]
    fn refocusing_a_pane_releases_the_launch_it_waited_for() {
        let mut fixture = Fixture::new();
        let token = fixture.state.create_activation_token();
        let pane = PaneId::from(1);
        fixture.state.focus_pane(
            pane,
            Target::Activation(token.clone()),
            Follow::First,
            size(),
        );

        fixture.state.focus_pane(
            pane,
            Target::Selector(Selector::AppId("app".into())),
            Follow::First,
            size(),
        );
        assert!(!fixture.state.protocols.launches.contains_key(&token));
    }

    #[test]
    fn refocusing_a_pane_on_the_same_launch_keeps_waiting_for_it() {
        let mut fixture = Fixture::new();
        let token = fixture.state.create_activation_token();
        let pane = PaneId::from(1);
        for _ in 0..2 {
            fixture.state.focus_pane(
                pane,
                Target::Activation(token.clone()),
                Follow::First,
                size(),
            );
        }
        assert!(fixture.state.protocols.launches.contains_key(&token));
        assert_eq!(
            fixture
                .state
                .attachments
                .get(pane)
                .and_then(AttachedPane::waiting),
            Some(&Target::Activation(token))
        );
    }
}
