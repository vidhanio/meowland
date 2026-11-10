//! Terminal events forwarded to the server by one session owner.

use meowland_input::{Event, Focus};
use meowland_protocol::{
    self as protocol, Input, PaneAction, PaneToServer, Transition, clipboard::MAX_TEXT,
};

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
                    self.send_input(input)?;
                }
            }
            Event::Text(text) => {
                if text.len() <= protocol::MAX_INPUT_TEXT {
                    self.send_input(Input::Text(text))?;
                }
            }
            Event::Focus(Focus::Gained) => {
                let input = self.keyboard.enter();
                self.send_input(input)?;
            }
            Event::Focus(Focus::Lost) => {
                self.keyboard.reset();
                self.send_input(Input::ResetKeyboard)?;
            }
            Event::Resize => {
                let cells = terminal::cells();
                let pixels = terminal::window_pixels();
                let cell = terminal::derived_cell(pixels, cells).or(self.cell);
                if cell != self.cell {
                    self.cell = cell;
                    self.presentation.cell_size_changed(&mut self.tx)?;
                }
                let (width, height) =
                    terminal::pane_pixels(None, pixels, cells, self.cell.unwrap_or(FALLBACK_CELL));
                let size = terminal::frame_size(width, height)?;
                self.tx
                    .send(&PaneToServer::Action(PaneAction::Resize { size }))?;
            }
            Event::Pointer(input) => self.send_input(input)?,
            Event::Paste(text) => {
                if text.len() <= MAX_TEXT {
                    self.tx
                        .send(&PaneToServer::Action(PaneAction::Paste(text)))?;
                } else {
                    tracing::warn!("Ignoring pasted text larger than {MAX_TEXT} bytes");
                }
            }
        }
        Ok(())
    }

    fn send_input(&mut self, input: Input) -> Result<()> {
        self.tx
            .send(&PaneToServer::Action(PaneAction::Input(input)))?;
        Ok(())
    }
}
