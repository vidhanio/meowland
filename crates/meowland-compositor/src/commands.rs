//! Pane commands and their Wayland side effects.

use meowland_core::pixels::FrameSize;
use meowland_protocol::{Follow, Input, PaneAction, Selector, Shortcut, Target, Transition};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use super::{
    AttachedPane, Compositor, PaneId, Selection, ServerToPane, WindowId, clipboard,
    input::pane_input,
};

/// The requested selector did not identify a mapped window to close.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no such window")]
pub struct CloseWindowError;

/// Owned operations applied exclusively on the Wayland event-loop thread.
#[derive(Clone, Debug)]
pub enum Command {
    /// Register a terminal pane with a window target and validated pixel
    /// bounds.
    Focus {
        /// Host-assigned pane identity.
        pane: PaneId,
        /// Windows the pane may show.
        target: Target,
        /// How long the pane keeps matching its target.
        follow: Follow,
        /// Physical-pixel bounds, validated during deserialization.
        size: FrameSize,
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
}

impl Compositor {
    /// Apply one pane or lifecycle command on the event-loop thread.
    pub fn handle_command(&mut self, command: Command) {
        match command {
            Command::Focus {
                pane,
                target,
                follow,
                size,
            } => self.focus_pane(pane, target, follow, size),
            Command::Detach { pane } => self.release_pane_window(pane),
            Command::Pane { pane, action } => self.pane_action(pane, action),
            Command::CloseAll => {
                for window in self.windows.by_id.values() {
                    window.surface.send_close();
                }
            }
        }
    }

    fn pane_action(&mut self, pane: PaneId, action: PaneAction) {
        match action {
            PaneAction::Resize { size } => self.resize_pane(pane, size),
            PaneAction::Input(event) => {
                if let Input::Key {
                    key,
                    modifiers,
                    state: Transition::Pressed,
                    ..
                } = &event
                    && let Some(shortcut) = self.input.shortcuts.action(*key, *modifiers)
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
                intent,
            } => clipboard::offer(
                self,
                pane,
                clipboard::TerminalOffer {
                    selection,
                    id: offer,
                    mimes,
                    intent,
                },
            ),
            PaneAction::ClipboardReply { request, data } => {
                clipboard::reply(self, pane, request, data);
            }
            PaneAction::ClipboardTransport(transport) => {
                self.attachments.set_clipboard_transport(pane, transport);
            }
            PaneAction::Ack(ack) => self.pane_ack(pane, ack),
            PaneAction::FrameTransport(transport) => {
                self.attachments.set_transport(pane, transport);
            }
        }
    }

    fn close_shown(&mut self, pane: PaneId) {
        if let Some(window) = self
            .attachments
            .get(pane)
            .and_then(AttachedPane::window)
            .and_then(|id| self.windows.by_id.get(&id))
        {
            window.surface.send_close();
        } else {
            self.emit(pane, ServerToPane::Release("empty pane".into()));
        }
    }

    /// Ask the selected mapped window, or the focused window, to close.
    ///
    /// # Errors
    /// Returns an error if the selector does not identify a mapped window.
    pub fn close_window(&self, selector: Option<&Selector>) -> Result<(), CloseWindowError> {
        let window = selector
            .map_or_else(
                || self.windows.catalog.focused(),
                |selector| self.windows.catalog.select(selector),
            )
            .and_then(|id| self.windows.by_id.get(&id))
            .ok_or(CloseWindowError)?;
        window.surface.send_close();
        Ok(())
    }

    pub(crate) fn selected_window(&self, selection: &Selection) -> Option<WindowId> {
        match selection {
            Selection::Matching(selector) => self.windows.catalog.select(selector),
            Selection::Activation(token) => self
                .protocols
                .launches
                .get(token)
                .and_then(|launch| launch.window)
                .filter(|&window| self.windows.catalog.drawable(window)),
        }
    }

    /// Publish the tiling, activation, and size state the compositor wants
    /// for a window, configuring the client only when it actually changed.
    pub(crate) fn apply_window_state(&self, window: WindowId) {
        let Some(entry) = self.windows.by_id.get(&window) else {
            return;
        };
        let activated = self.windows.catalog.focused() == Some(window);
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
