//! Commands entering the display thread and their Wayland side effects.

use meowland_core::pixels::FrameSize;
use meowland_protocol::Show;
use smithay::{
    output::Mode, reexports::wayland_protocols::xdg::shell::server::xdg_toplevel, utils::Size,
};

use super::{
    Binding, Command, Event, Pane, PaneId, Selection, State, WindowId, clipboard, input::pane_input,
};

impl State {
    pub fn handle_command(&mut self, command: Command) {
        match command {
            Command::CreateActivationToken { request } => {
                let token = self.create_activation_token();
                if self
                    .events
                    .send(Event::ActivationCreated {
                        request,
                        token: token.clone(),
                    })
                    .is_err()
                {
                    self.cancel_activation(&token, "launch cancelled");
                }
            }
            Command::CancelActivation { token } => {
                self.cancel_activation(&token, "launch cancelled");
            }
            Command::Kill {
                request,
                show,
                waiting,
                deadline,
            } => {
                if waiting.load(std::sync::atomic::Ordering::Relaxed)
                    && std::time::Instant::now() < deadline
                {
                    let result = self.close_window(&show);
                    let _ = self.events.send(Event::CloseRequested { request, result });
                }
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
                if let Some(size) = FrameSize::new(width, height) {
                    self.resize_pane(pane, size);
                }
            }
            Command::Input { pane, event } => pane_input(self, pane, &event),
            Command::Paste { pane, text } => clipboard::paste(self, pane, &text),
            Command::ClipboardOffer {
                pane,
                offer,
                mimes,
                paste,
            } => clipboard::offer(self, pane, offer, mimes, paste),
            Command::ClipboardReply {
                pane,
                request,
                data,
            } => clipboard::reply(self, pane, request, data),
            Command::Ack { pane, drawn } => self.pane_ack(pane, drawn),
            Command::CloseShown { pane } => {
                if let Some(window) = self
                    .attachments
                    .panes
                    .get(&pane)
                    .and_then(Pane::window)
                    .and_then(|id| self.windows.get(&id))
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

    fn resize_pane(&mut self, pane: PaneId, size: FrameSize) {
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

    fn attach_pane(&mut self, pane: PaneId, show: Show, size: FrameSize) {
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

    fn close_window(&self, show: &Show) -> Result<(), String> {
        let window = self
            .selected_window(show)
            .and_then(|id| self.windows.get(&id))
            .ok_or_else(|| "no such window".to_owned())?;
        window.surface.send_close();
        Ok(())
    }

    fn selected_window(&self, show: &Show) -> Option<WindowId> {
        let activation = match show {
            Show::Activation(token) => self.launches.get(token).and_then(|launch| launch.window),
            _ => None,
        };
        self.catalog.select(show, activation)
    }

    fn show_pane_window(&mut self, pane: PaneId, window: WindowId) {
        let token = self
            .attachments
            .panes
            .get_mut(&pane)
            .and_then(|entry| entry.show(window));
        if let Some(token) = token {
            self.forget_activation(&token);
        }
        self.enter_output(window);
        self.send_title(pane, window);
        self.send_pane_cursor(pane);
        self.apply_window_state(window);
        self.focus_window(window);
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

    pub fn apply_window_state(&self, window: WindowId) {
        let Some(entry) = self.windows.get(&window) else {
            return;
        };
        let activated = self.catalog.focused() == Some(window);
        let size = self.pane_size(window);
        entry.surface.with_pending_state(|pending| {
            pending.size = Some(size);
            pending.bounds = Some(size);
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

    pub fn configure_output(&mut self, size: FrameSize) {
        let mode = Mode {
            size: (size.width() as i32, size.height() as i32).into(),
            refresh: 60_000,
        };
        self.mode = Size::from((mode.size.w, mode.size.h));
        for previous in self.output.modes() {
            if previous != mode {
                self.output.delete_mode(previous);
            }
        }
        self.output.set_preferred(mode);
        self.output
            .change_current_state(Some(mode), None, None, None);
    }
}
