//! Pane commands and their Wayland side effects.

use meowland_core::pixels::FrameSize;
use meowland_protocol::Show;
use smithay::{output::Mode, reexports::wayland_protocols::xdg::shell::server::xdg_toplevel};

use super::{Command, Pane, ServerToPane, State, WindowId, clipboard, input::pane_input};

impl State {
    /// Apply one pane or lifecycle command on the event-loop thread.
    pub fn handle_command(&mut self, command: Command) {
        match command {
            Command::Attach {
                pane,
                show,
                width,
                height,
            } => {
                let Some(size) = FrameSize::new(width, height) else {
                    self.emit(
                        pane,
                        ServerToPane::Release("invalid pane dimensions".into()),
                    );
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
                selection,
                offer,
                mimes,
                paste,
            } => clipboard::offer(self, pane, selection, offer, mimes, paste),
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
                    self.emit(pane, ServerToPane::Release("empty pane".into()));
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

    /// Ask the selected mapped window to close.
    ///
    /// # Errors
    /// Returns an error if the selection does not identify a mapped window.
    pub fn close_window(&self, show: &Show) -> Result<(), String> {
        let window = self
            .selected_window(show)
            .and_then(|id| self.windows.get(&id))
            .ok_or_else(|| "no such window".to_owned())?;
        window.surface.send_close();
        Ok(())
    }

    pub(crate) fn selected_window(&self, show: &Show) -> Option<WindowId> {
        let activation = match show {
            Show::Activation(token) => self.launches.get(token).and_then(|launch| launch.window),
            _ => None,
        };
        self.catalog.select(show, activation)
    }

    pub(crate) fn apply_window_state(&self, window: WindowId) {
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

    pub(crate) fn configure_output(&mut self, size: FrameSize) {
        let mode = Mode {
            size: (size.width() as i32, size.height() as i32).into(),
            refresh: 60_000,
        };
        self.mode = self.logical_size(size.width(), size.height());
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
