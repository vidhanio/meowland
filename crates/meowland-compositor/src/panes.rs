//! Pane attachment, window ownership, and presentation.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, Selector, Target};
use smithay::utils::{Logical, Size};

use super::{Compositor, PaneId, ServerToPane, WindowId, frame::PaneState, input};

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

    /// Mark the content of the pane owning a window as stale.
    pub(crate) fn touch(&mut self, window: WindowId) {
        if let Some(pane) = self.attachments.owner(window)
            && let Some(state) = self.attachments.state_mut(pane)
        {
            state.mark_dirty();
        }
    }

    /// Whether any pane has a frame waiting for dispatch.
    pub(crate) fn frames_due(&self) -> bool {
        self.attachments.iter().any(|(_, entry)| entry.state.due())
    }

    /// Remove every pane-keyed entry, returning the window the pane owned.
    pub(crate) fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        if self.input.keyboard_state.owned_by(pane) {
            input::reset_keyboard(self);
        }
        let removed = self.attachments.remove(pane)?;
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
        if let Some(Target::Activation(token)) = removed.waiting() {
            self.release_activation(token);
        }
        let window = removed.window();
        if let Some(window) = window {
            self.leave_output(window);
        }
        window
    }

    /// Remove a pane and ask the window it owned to close.
    pub(crate) fn close_pane(&mut self, pane: PaneId) {
        if let Some(window) = self.remove_pane(pane)
            && let Some(entry) = self.windows.by_id.get(&window)
        {
            entry.surface.send_close();
        }
    }

    /// Remove a pane, leaving its window open without an owner for a waiting
    /// or later pane to claim. Focus moves to an owned window when the
    /// released window had it.
    pub(crate) fn detach_pane(&mut self, pane: PaneId) {
        let Some(window) = self.remove_pane(pane) else {
            return;
        };
        self.apply_window_state(window);
        self.resolve_pending_panes();
        if self.windows.catalog.focused() == Some(window)
            && self.attachments.owner(window).is_none()
        {
            self.restore_focus();
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
        self.attachments
            .insert(pane, target, follow, PaneState::new(size));
        match self.selected_window(pane) {
            Some(window) => self.show_pane_window(pane, window),
            None if follow == Follow::Current => {
                self.remove_pane(pane);
                self.emit(pane, ServerToPane::Release("no such window".into()));
            }
            None => {}
        }
    }

    /// The newest window a waiting pane's target matches that no other pane
    /// owns.
    fn selected_window(&self, pane: PaneId) -> Option<WindowId> {
        let available = |window| self.attachments.claimable(pane, window);
        match self.attachments.get(pane)?.waiting()? {
            Target::Selector(selector) => self.windows.catalog.select_where(selector, available),
            Target::Activation(token) => self
                .protocols
                .launches
                .get(token)
                .and_then(|launch| launch.window)
                .filter(|&window| self.windows.catalog.drawable(window) && available(window)),
        }
    }

    /// Make a pane own a window and focus it. A pane following every match
    /// of a launch keeps matching the activated window's app ID afterwards.
    fn show_pane_window(&mut self, pane: PaneId, window: WindowId) {
        let Some(follow) = self.attachments.get(pane).map(|entry| entry.follow) else {
            return;
        };
        if !self.attachments.claimable(pane, window) {
            return;
        }
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
        if self.input.keyboard_state.owned_by(pane) {
            input::reset_keyboard(self);
        }
        self.resume_callbacks(window);
        self.enter_output(window);
        self.send_title(pane, window);
        self.send_pane_cursor(pane);
        self.apply_window_state(window);
        self.focus_window(window);
    }

    /// Give each waiting pane the newest unowned window its target matches,
    /// in pane-identity order.
    pub(crate) fn resolve_pending_panes(&mut self) {
        let waiting: Vec<_> = self.attachments.waiting().collect();
        for pane in waiting {
            if let Some(window) = self.selected_window(pane) {
                self.show_pane_window(pane, window);
            }
        }
    }

    /// When the pane owning a window that went away follows every match,
    /// move it to another unowned match, or tell it to wait for one.
    pub(crate) fn unbind_following_pane(&mut self, window: WindowId) {
        let Some(pane) = self.attachments.owner(window).filter(|&pane| {
            self.attachments
                .get(pane)
                .is_some_and(|entry| entry.follow == Follow::Every)
        }) else {
            return;
        };
        self.attachments.unbind(pane);
        if let Some(state) = self.attachments.state_mut(pane) {
            state.mark_dirty();
        }
        self.resolve_pending_panes();
        if self
            .attachments
            .get(pane)
            .is_some_and(|entry| entry.window().is_none())
        {
            self.emit(pane, ServerToPane::Waiting);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttachedPane, fixture::Fixture};
    use wayland_client::Proxy as _;

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

    fn size() -> FrameSize {
        FrameSize::new(64, 64).expect("test size")
    }

    fn app() -> Target {
        Target::Selector(Selector::AppId("app".into()))
    }

    #[test]
    fn an_owned_window_is_claimed_by_another_pane_only_once_detached() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let window = fixture.window(&toplevel);
        let (owner, current, follower) = (PaneId::from(1), PaneId::from(2), PaneId::from(3));
        fixture
            .state
            .focus_pane(owner, app(), Follow::Current, size());
        assert_eq!(shown(&fixture, owner), Some(window));

        fixture.state.drain_events().for_each(drop);
        fixture
            .state
            .focus_pane(current, app(), Follow::Current, size());
        assert!(!fixture.state.attachments.contains(current));
        assert!(
            fixture.state.drain_events().any(|event| event.pane == current
                && matches!(event.message, ServerToPane::Release(_)))
        );

        fixture
            .state
            .focus_pane(follower, app(), Follow::Every, size());
        assert_eq!(shown(&fixture, follower), None);

        fixture.state.detach_pane(owner);
        fixture.roundtrip();
        assert_eq!(shown(&fixture, follower), Some(window));
        assert!(fixture.app.closed.is_empty(), "detaching keeps the window");
    }

    #[test]
    fn closing_a_pane_asks_its_window_to_close() {
        let mut fixture = Fixture::new();
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let pane = PaneId::from(1);
        fixture
            .state
            .focus_pane(pane, app(), Follow::Current, size());

        fixture.state.close_pane(pane);
        fixture.roundtrip();
        assert!(!fixture.state.attachments.contains(pane));
        assert_eq!(fixture.app.closed, [toplevel.role.id()]);
    }

    #[test]
    fn waiting_panes_claim_distinct_windows() {
        let mut fixture = Fixture::new();
        let (first, second) = (PaneId::from(1), PaneId::from(2));
        for pane in [first, second] {
            fixture.state.focus_pane(pane, app(), Follow::Every, size());
        }

        let one = fixture.toplevel("app");
        fixture.present(&one.surface, 4, 4, [0, 0, 0, 255]);
        let one = fixture.window(&one);
        assert_eq!(shown(&fixture, first), Some(one));
        assert_eq!(shown(&fixture, second), None);

        let two = fixture.toplevel("app");
        fixture.present(&two.surface, 4, 4, [0, 0, 0, 255]);
        let two = fixture.window(&two);
        assert_eq!(shown(&fixture, first), Some(one));
        assert_eq!(shown(&fixture, second), Some(two));
    }

    #[test]
    fn only_the_pane_receiving_input_shows_the_application_cursor() {
        let mut fixture = Fixture::new();
        let (first, second) = (PaneId::from(1), PaneId::from(2));
        for (pane, app_id) in [(first, "first"), (second, "second")] {
            let toplevel = fixture.toplevel(app_id);
            fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
            fixture.state.focus_pane(
                pane,
                Target::Selector(Selector::AppId(app_id.into())),
                Follow::Current,
                size(),
            );
        }
        fixture.state.input.cursor.shape = Some("text".into());
        let pointer = meowland_protocol::Input::Pointer {
            x: 1.0,
            y: 1.0,
            button: None,
            scroll: meowland_protocol::Scroll::default(),
        };
        let cursors = |fixture: &mut Fixture, pane: PaneId| {
            fixture.state.drain_events().for_each(drop);
            input::pane_input(&mut fixture.state, pane, &pointer);
            fixture
                .state
                .drain_events()
                .filter_map(|event| match event.message {
                    ServerToPane::Cursor(shape) => Some((event.pane, shape.is_some())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(cursors(&mut fixture, first), [(first, true)]);
        assert_eq!(
            cursors(&mut fixture, second),
            [(first, false), (second, true)]
        );
    }

    #[test]
    fn following_pane_waits_without_frames_until_a_new_match_maps() {
        let mut fixture = Fixture::new();
        let first = fixture.toplevel("app");
        fixture.present(&first.surface, 4, 4, [0, 0, 0, 255]);
        let pane = PaneId::from(1);
        fixture.state.focus_pane(pane, app(), Follow::Every, size());
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
    fn detaching_skips_an_unmapped_window_when_restoring_focus() {
        let mut fixture = Fixture::new();
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
        fixture.state.detach_pane(PaneId::from(3));
        assert_eq!(fixture.state.windows.catalog.focused(), Some(windows[1]));
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
}
