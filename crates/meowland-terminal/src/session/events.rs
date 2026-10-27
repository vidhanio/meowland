//! Terminal events and clipboard actions coordinated by one session owner.

use meowland_core::Result;
use meowland_input::{Event, keyboard};
use meowland_protocol::{
    self as protocol, Input, PaneAction, PaneToServer, Shortcut,
    clipboard::{MAX_TEXT, Selection},
};
use meowland_terminal_clipboard::{Action, Focus, Refresh};

use super::{
    TerminalSession,
    terminal::{self, FALLBACK_CELL},
};

impl TerminalSession {
    pub(super) fn send_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) => {
                if let Some(
                    input @ Input::Key {
                        code,
                        pressed,
                        modifiers,
                        ..
                    },
                ) = self.keyboard.input(key)
                {
                    if pressed && Shortcut::from_key(code, modifiers).is_some() {
                        self.keyboard.intercept(code);
                    }
                    let defer = if pressed && keyboard::paste(code, modifiers) {
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
                self.clipboard.focus(focus);
                if focused {
                    self.input_gate
                        .send(self.keyboard.enter(), None, &mut self.tx)?;
                } else {
                    self.keyboard.reset();
                    self.input_gate.reset();
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
                self.tx
                    .send(&PaneToServer::Action(PaneAction::Resize { width, height }))?;
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
                    self.tx
                        .send(&PaneToServer::Action(PaneAction::ClipboardOffer {
                            selection,
                            offer,
                            mimes,
                            paste: paste && !deferred_paste,
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
