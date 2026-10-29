//! Pane commands and their Wayland side effects.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Input, PaneAction, Shortcut, Show};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use super::{AttachedPane, PaneId, ServerToPane, State, WindowId, clipboard, input::pane_input};

/// The requested selector did not identify a mapped window to close.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no such window")]
pub struct CloseWindowError;

/// Owned operations applied exclusively on the Wayland event-loop thread.
#[derive(Clone, Debug)]
pub enum Command {
    /// Register a terminal pane with a window selector and validated pixel
    /// bounds.
    Attach {
        /// Host-assigned pane identity.
        pane: PaneId,
        /// Window to select or follow.
        show: Show,
        /// Physical-pixel width, validated before attachment.
        width: u32,
        /// Physical-pixel height, validated before attachment.
        height: u32,
    },
    /// Remove pane state without closing its Wayland window.
    Detach {
        /// Pane to detach.
        pane: PaneId,
    },
    /// Apply input, geometry, clipboard, or acknowledgement work for an
    /// attached pane.
    Pane {
        /// Pane originating the action.
        pane: PaneId,
        /// Work to apply on the Wayland thread.
        action: PaneAction,
    },
    /// Ask all windows to close while continuing to dispatch their shutdown
    /// work.
    CloseAll,
    /// Ask windows to close and stop event-loop dispatch.
    Shutdown,
}

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
            Command::Pane { pane, action } => self.pane_action(pane, action),
            command @ (Command::CloseAll | Command::Shutdown) => {
                for window in self.windows.values() {
                    window.surface.send_close();
                }
                self.shutdown |= matches!(command, Command::Shutdown);
            }
        }
    }

    fn pane_action(&mut self, pane: PaneId, action: PaneAction) {
        match action {
            PaneAction::Resize { width, height } => {
                if let Some(size) = FrameSize::new(width, height) {
                    self.resize_pane(pane, size);
                }
            }
            PaneAction::Input(event) => {
                if let Input::Key {
                    code,
                    modifiers,
                    pressed: true,
                    ..
                } = &event
                    && let Some(shortcut) = Shortcut::from_key(*code, *modifiers)
                {
                    match shortcut {
                        Shortcut::CloseWindow => self.close_shown(pane),
                        Shortcut::DetachPane => {
                            self.release_pane_window(pane);
                            self.emit(pane, ServerToPane::Release("detached".into()));
                        }
                    }
                } else {
                    pane_input(self, pane, &event);
                }
            }
            PaneAction::Paste(text) => clipboard::paste(self, pane, &text),
            PaneAction::ClipboardOffer {
                selection,
                offer,
                mimes,
                paste,
            } => clipboard::offer(
                self,
                pane,
                clipboard::TerminalOffer {
                    selection,
                    id: offer,
                    mimes,
                    paste,
                },
            ),
            PaneAction::ClipboardReply { request, data } => {
                clipboard::reply(self, pane, request, data);
            }
            PaneAction::Ack(ack) => self.pane_ack(pane, ack),
            PaneAction::FrameTransport(transport) => {
                if let Some(entry) = self.attachments.get_mut(pane) {
                    entry.presentation.transport = transport;
                }
            }
        }
    }

    fn close_shown(&mut self, pane: PaneId) {
        if let Some(window) = self
            .attachments
            .get(pane)
            .and_then(AttachedPane::window)
            .and_then(|id| self.windows.get(&id))
        {
            window.surface.send_close();
        } else {
            self.emit(pane, ServerToPane::Release("empty pane".into()));
        }
    }

    /// Ask the selected mapped window to close.
    ///
    /// # Errors
    /// Returns an error if the selection does not identify a mapped window.
    pub fn close_window(&self, show: &Show) -> Result<(), CloseWindowError> {
        let window = self
            .selected_window(show)
            .and_then(|id| self.windows.get(&id))
            .ok_or(CloseWindowError)?;
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

    /// Publish the tiling, activation, and size state the compositor wants
    /// for a window, configuring the client only when it actually changed.
    pub(crate) fn apply_window_state(&self, window: WindowId) {
        let Some(entry) = self.windows.get(&window) else {
            return;
        };
        let activated = self.catalog.focused() == Some(window);
        let size = self.pane_size(window);
        entry.surface.with_pending_state(|pending| {
            pending.size = Some(size);
            pending.bounds = Some(size);
            pending.states.set(xdg_toplevel::State::TiledLeft);
            pending.states.set(xdg_toplevel::State::TiledRight);
            pending.states.set(xdg_toplevel::State::TiledTop);
            pending.states.set(xdg_toplevel::State::TiledBottom);
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
        let _ = entry.surface.send_pending_configure();
    }
}
