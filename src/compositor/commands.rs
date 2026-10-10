//! Pane attachment and compositor commands; all mutations stay on the display
//! thread.

use smithay::{
    output::Mode, reexports::wayland_protocols::xdg::shell::server::xdg_toplevel, utils::Size,
};

use super::{Command, Event, State, clipboard, frame::PaneState, input::pane_input};
use crate::{pixels::FrameSize, protocol::Show};

impl State {
    pub(super) fn handle_command(&mut self, command: Command) {
        match command {
            Command::CreateActivationToken { reply } => {
                let token = self.create_activation_token();
                if reply.send(token.clone()).is_err() {
                    self.cancel_activation(&token, "launch cancelled");
                }
            }
            Command::CancelActivation { token } => {
                self.cancel_activation(&token, "launch cancelled");
            }
            Command::Kill { show, reply } => {
                let _ = reply.send(self.close_window(&show));
            }
            Command::Attach {
                pane,
                show,
                width,
                height,
            } => {
                let Some(size) = FrameSize::new(width, height) else {
                    let _ = self.events.send(Event::Release {
                        pane,
                        reason: "invalid pane dimensions".into(),
                    });
                    return;
                };
                self.attach_pane(pane, show, size);
            }
            Command::Detach { pane } => self.release_pane_window(pane),
            Command::Resize {
                pane,
                width,
                height,
            } => {
                let Some(size) = FrameSize::new(width, height) else {
                    return;
                };
                let Some(entry) = self.panes.get_mut(&pane) else {
                    return;
                };
                entry.size = size;
                entry.mark_dirty();
                let window = entry.window;
                if self.panes.keys().min() == Some(&pane) {
                    self.configure_output(size);
                }
                self.apply_window_state(window);
            }
            Command::Input { pane, event } => pane_input(self, pane, &event),
            Command::Paste { pane, text } => clipboard::paste(self, pane, &text),
            Command::ClipboardOffer {
                pane,
                offer,
                mimes,
                paste,
            } => {
                clipboard::offer(self, pane, offer, mimes, paste);
            }
            Command::ClipboardReply {
                pane,
                request,
                data,
            } => {
                clipboard::reply(self, pane, request, data);
            }
            Command::Ack { pane, drawn } => self.pane_ack(pane, drawn),
            Command::CloseShown { pane } => {
                if let Some(window) = self
                    .panes
                    .get(&pane)
                    .and_then(|pane| self.windows.get(&pane.window))
                {
                    window.surface.send_close();
                } else {
                    let _ = self.events.send(Event::Release {
                        pane,
                        reason: "empty pane".into(),
                    });
                }
            }
            command @ (Command::CloseAll | Command::Shutdown) => {
                for window in self.windows.values() {
                    window.surface.send_close();
                }
                self.shutdown |= matches!(command, Command::Shutdown);
            }
        }
    }

    /// Explicit app IDs and launch tokens wait for their window, never the
    /// newest.
    fn attach_pane(&mut self, pane: u64, show: Show, size: FrameSize) {
        if matches!(&show, Show::Activation(token) if !self.launches.contains_key(token)) {
            let _ = self.events.send(Event::Release {
                pane,
                reason: "no such activation".into(),
            });
            return;
        }
        if matches!(show, Show::Newest) {
            self.following.insert(pane);
        }
        if let Some(window) = self.selected_window(&show) {
            self.insert_pane(pane, PaneState::new(window, size));
            self.show_pane_window(pane, window);
            if let Show::Activation(token) = show {
                self.forget_activation(&token);
            }
        } else if matches!(show, Show::Id(_)) {
            let _ = self.events.send(Event::Release {
                pane,
                reason: "no such window".into(),
            });
        } else {
            self.insert_pane(pane, PaneState::new(0, size));
            match show {
                Show::AppId(_) | Show::Activation(_) => {
                    self.pending_shows.insert(pane, show);
                }
                Show::Newest | Show::Focused => {
                    self.following.insert(pane);
                }
                Show::Id(_) => unreachable!(),
            }
        }
    }

    fn close_window(&self, show: &Show) -> Result<(), String> {
        let window = self
            .selected_window(show)
            .and_then(|window| self.windows.get(&window))
            .ok_or_else(|| "no such window".to_owned())?;
        window.surface.send_close();
        Ok(())
    }

    fn selected_window(&self, show: &Show) -> Option<u64> {
        let window = match show {
            Show::Id(id) => Some(*id),
            Show::Newest => self.newest,
            Show::Focused => self.focused,
            Show::AppId(app_id) => self
                .windows
                .iter()
                .filter(|(_, window)| window.announced && window.app_id == *app_id)
                .map(|(id, _)| *id)
                .max(),
            Show::Activation(token) => self.launches.get(token).and_then(|launch| launch.window),
        }?;
        self.windows
            .get(&window)
            .filter(|window| window.announced)
            .map(|_| window)
    }

    fn show_pane_window(&mut self, pane: u64, window: u64) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.window = window;
            entry.mark_dirty();
        }
        self.enter_output(window);
        self.send_title(pane, window);
        self.apply_window_state(window);
        self.focus_window(window);
    }

    /// Called on mapping, metadata changes and activation, not on a timer.
    pub(super) fn resolve_pending_panes(&mut self) {
        let ready: Vec<_> = self
            .pending_shows
            .iter()
            .filter_map(|(pane, show)| self.selected_window(show).map(|window| (*pane, window)))
            .collect();
        for (pane, window) in ready {
            let show = self.pending_shows.remove(&pane);
            self.show_pane_window(pane, window);
            if let Some(Show::Activation(token)) = show {
                self.forget_activation(&token);
            }
        }
    }

    pub(super) fn apply_window_state(&self, window: u64) {
        let Some(entry) = self.windows.get(&window) else {
            return;
        };
        let activated = self.focused == Some(window);
        let size = self.pane_size(window);
        entry.surface.with_pending_state(|pending| {
            pending.size = Some(size);
            pending.states.set(xdg_toplevel::State::Maximized);
            if activated {
                pending.states.set(xdg_toplevel::State::Activated);
            } else {
                pending.states.unset(xdg_toplevel::State::Activated);
            }
            if entry.fullscreen {
                pending.states.set(xdg_toplevel::State::Fullscreen);
            } else {
                pending.states.unset(xdg_toplevel::State::Fullscreen);
            }
        });
        entry.surface.send_configure();
    }

    pub(super) fn configure_output(&mut self, size: FrameSize) {
        let mode = Mode {
            size: (size.width() as i32, size.height() as i32).into(),
            refresh: 60_000,
        };
        self.mode = Size::from((mode.size.w, mode.size.h));
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
    }
}
