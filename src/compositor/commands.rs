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
            Command::Paste { pane, text } => clipboard::paste(self, pane, text),
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

    /// A missing explicit ID is released; a dynamic selection waits for pixels.
    fn attach_pane(&mut self, pane: u64, show: Show, size: FrameSize) {
        if matches!(show, Show::Newest) {
            self.following.insert(pane);
        }
        let id = match show {
            Show::Id(id) => id,
            Show::Newest => self.newest.unwrap_or(0),
            Show::Focused => self.focused.unwrap_or(0),
        };
        let announced = self.windows.get(&id).is_some_and(|window| window.announced);
        if announced {
            self.insert_pane(pane, PaneState::new(id, size));
            self.enter_output(id);
            self.send_title(pane, id);
            self.mark_dirty(pane);
            self.apply_window_state(id);
            self.focus_window(id);
        } else if matches!(show, Show::Newest | Show::Focused) {
            self.insert_pane(pane, PaneState::new(0, size));
            self.following.insert(pane);
        } else {
            let _ = self.events.send(Event::Release {
                pane,
                reason: "no such window".into(),
            });
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
