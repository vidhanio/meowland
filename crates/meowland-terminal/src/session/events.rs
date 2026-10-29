//! Terminal events and clipboard actions coordinated by one session owner.

use meowland_input::{Event, keyboard};
use meowland_protocol::{
    self as protocol, Input, OfferIntent, PaneAction, PaneToServer, Transition,
    clipboard::{MAX_TEXT, Selection},
};
use meowland_terminal_clipboard::{Action, Focus, Refresh};

use super::{
    TerminalSession,
    terminal::{self, FALLBACK_CELL},
};
use crate::Result;

impl TerminalSession {
    pub(super) fn send_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(event) => {
                if let Some(
                    input @ Input::Key {
                        key,
                        state,
                        modifiers,
                        ..
                    },
                ) = self.keyboard.input(event)
                {
                    let pressed = state == Transition::Pressed;
                    if pressed && self.shortcuts.action(key, modifiers).is_some() {
                        self.keyboard.intercept(key);
                    }
                    let defer = if pressed && keyboard::paste(key, modifiers) {
                        self.clipboard.refresh(Selection::Clipboard, Refresh::Paste)
                    } else {
                        None
                    };
                    self.input_gate.send(input, defer, &mut self.tx)?;
                }
            }
            Event::Text(text) => {
                if text.len() <= protocol::MAX_INPUT_TEXT {
                    self.input_gate
                        .send(Input::Text(text), None, &mut self.tx)?;
                }
            }
            Event::Focus(focused) => {
                let focus = if focused { Focus::Gained } else { Focus::Lost };
                let preserve = self.clipboard.focus(focus);
                if focused {
                    self.input_gate
                        .send(self.keyboard.enter(), None, &mut self.tx)?;
                } else {
                    self.keyboard.reset();
                    if preserve {
                        self.input_gate.preserve_focus_loss();
                    } else {
                        self.input_gate.reset();
                    }
                    self.tx.send(&PaneToServer::Action(PaneAction::Input(
                        Input::ResetKeyboard,
                    )))?;
                }
            }
            Event::Resize => {
                let cells = terminal::cells();
                let pixels = terminal::window_pixels();
                self.cell = terminal::derived_cell(pixels, cells).or(self.cell);
                self.presentation.set_cell_size(self.cell);
                let (width, height) =
                    terminal::pane_pixels(None, pixels, cells, self.cell.unwrap_or(FALLBACK_CELL));
                let size = terminal::frame_size(width, height)?;
                self.tx
                    .send(&PaneToServer::Action(PaneAction::Resize { size }))?;
            }
            Event::Pointer(input) => self.input_gate.send(input, None, &mut self.tx)?,
            Event::Paste(text) => {
                self.clipboard
                    .cancel_refresh_selection(Selection::Clipboard);
                self.input_gate.flush(&mut self.tx)?;
                if text.len() <= MAX_TEXT {
                    self.tx
                        .send(&PaneToServer::Action(PaneAction::Paste(text)))?;
                } else {
                    tracing::warn!("Ignoring pasted text larger than {MAX_TEXT} bytes");
                }
            }
            Event::Clipboard(bytes) => self.clipboard.packet(&bytes),
            Event::ClipboardText(bytes) => self.clipboard.text(&bytes),
        }
        Ok(())
    }

    pub(super) fn send_clipboard_actions(&mut self) -> Result<()> {
        while let Some(action) = self.clipboard.next_action() {
            match action {
                Action::Offer {
                    selection,
                    offer,
                    mimes,
                    paste,
                } => {
                    let deferred_paste = paste && self.input_gate.blocked();
                    let intent = if paste && !deferred_paste {
                        OfferIntent::Paste
                    } else {
                        OfferIntent::Refresh
                    };
                    self.tx
                        .send(&PaneToServer::Action(PaneAction::ClipboardOffer {
                            selection,
                            offer,
                            mimes,
                            intent,
                        }))?;
                    if deferred_paste {
                        self.input_gate.flush(&mut self.tx)?;
                    }
                }
                Action::Reply { request, data } => {
                    self.tx
                        .send(&PaneToServer::Action(PaneAction::ClipboardReply {
                            request,
                            data,
                        }))?;
                }
                Action::Unblock => self.input_gate.flush(&mut self.tx)?,
            }
        }
        Ok(())
    }
}
