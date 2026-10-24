//! Terminal input, graphics, and mode management for a pane.

use std::{path::Path, time::Instant};

use meowland_core::{Error, Result};
use meowland_input::{self as input, keyboard};
use meowland_protocol::{
    self as protocol, Input, PaneToServer, Show,
    asynchronous::{Outgoing, Reader},
    clipboard::{MAX_TEXT, Selection},
};
use meowland_terminal_clipboard as clipboard;

mod output;
mod presentation;
mod probe;
mod session;
mod terminal;

use clipboard::{Action as ClipboardAction, Clipboard};
use input::Event;
use keyboard::Keyboard;
use presentation::PanePresentation;
use terminal::FALLBACK_CELL;

/// Attach this terminal to a pane until release or disconnection.
///
/// This invocation exclusively owns terminal input and raw mode. Do not read
/// stdin concurrently; a separate nonblocking descriptor reads the same
/// terminal. Terminal modes and capability probing begin only when the server
/// supplies the selected window's first frame; waiting leaves the normal screen
/// and terminal signal handling intact.
///
/// # Errors
/// Fails if the terminal cannot be configured or probed, or the pane
/// connection fails. Server-initiated release restores the terminal and
/// returns `Ok`.
pub async fn attach(socket: &Path, show: Show) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| {
            Error::io(
                format!(
                    "could not reach the pane socket {} (is the server running?)",
                    socket.display()
                ),
                error,
            )
        })?;
    let (reader, writer) = stream.into_split();
    session::TerminalSession::new(Reader::new(reader), writer, show)?
        .run()
        .await
}

fn send_event(
    event: Event,
    tx: &mut Outgoing,
    presentation: &mut PanePresentation,
    cell: &mut Option<(u16, u16)>,
    clipboard: &mut Clipboard,
    keyboard: &mut Keyboard,
) -> Result<()> {
    match event {
        Event::Key(key) => {
            if let Some(
                input @ Input::Key {
                    code,
                    pressed,
                    modifiers,
                    ..
                },
            ) = keyboard.input(key)
            {
                if pressed && keyboard::binding(code, modifiers) {
                    keyboard.intercept(code);
                }
                let defer = keyboard.deadline().or_else(|| {
                    if pressed && keyboard::paste(code, modifiers) {
                        clipboard.refresh(Selection::Clipboard, true)
                    } else {
                        None
                    }
                });
                send_input(input, defer, keyboard, tx)?;
            }
        }
        Event::Text(text) => {
            if text.len() <= protocol::MAX_INPUT_TEXT {
                send_input(Input::Text(text), keyboard.deadline(), keyboard, tx)?;
            }
        }
        Event::Focus(focused) => {
            let pending_paste = clipboard.focus(focused);
            if focused {
                send_input(keyboard.enter(), keyboard.deadline(), keyboard, tx)?;
            } else {
                keyboard.reset(pending_paste);
                tx.send(&PaneToServer::Input(Input::ResetKeyboard))?;
            }
        }
        Event::Resize => {
            let cells = terminal::cells();
            let pixels = terminal::window_pixels();
            *cell = terminal::derived_cell(pixels, cells).or(*cell);
            presentation.set_cell_size(*cell);
            let (width, height) =
                terminal::pane_pixels(None, pixels, cells, cell.unwrap_or(FALLBACK_CELL));
            tx.send(&PaneToServer::Resize { width, height })?;
        }
        Event::Pointer(input) => send_input(input, keyboard.deadline(), keyboard, tx)?,
        Event::Paste(text) => {
            clipboard.cancel_refresh_selection(Selection::Clipboard);
            send_deferred(keyboard, tx)?;
            send_paste(tx, text)?;
        }
        Event::Clipboard(bytes) => clipboard.packet(&bytes),
        Event::ClipboardText(bytes) => clipboard.text(&bytes),
    }
    Ok(())
}

fn send_input(
    input: Input,
    defer: Option<Instant>,
    keyboard: &mut Keyboard,
    tx: &mut Outgoing,
) -> Result<()> {
    if let Some(deadline) = defer {
        keyboard.defer(input, deadline);
        if keyboard.deferred_len() >= 256 {
            tracing::warn!("Clipboard refresh deferred too much input; forwarding original events");
            send_deferred(keyboard, tx)?;
        }
    } else {
        tx.send(&PaneToServer::Input(input))?;
    }
    Ok(())
}

fn send_clipboard_actions(
    clipboard: &mut Clipboard,
    keyboard: &mut Keyboard,
    tx: &mut Outgoing,
) -> Result<()> {
    while let Some(action) = clipboard.next_action() {
        match action {
            ClipboardAction::Offer {
                selection,
                offer,
                mimes,
                paste,
            } => {
                if paste && keyboard.deferred() {
                    tx.send(&PaneToServer::ClipboardOffer {
                        selection,
                        offer,
                        mimes: mimes.clone(),
                        paste: false,
                    })?;
                    send_deferred(keyboard, tx)?;
                }
                tx.send(&PaneToServer::ClipboardOffer {
                    selection,
                    offer,
                    mimes,
                    paste,
                })?;
            }
            ClipboardAction::Reply { request, data } => {
                tx.send(&PaneToServer::ClipboardReply { request, data })?;
            }
            ClipboardAction::Unblock => send_deferred(keyboard, tx)?,
        }
    }
    Ok(())
}

fn send_deferred(keyboard: &mut Keyboard, tx: &mut Outgoing) -> Result<()> {
    for input in keyboard.take_deferred() {
        tx.send(&PaneToServer::Input(input))?;
    }
    Ok(())
}

fn send_paste(tx: &mut Outgoing, text: String) -> Result<()> {
    if text.len() <= MAX_TEXT {
        tx.send(&PaneToServer::Paste(text))?;
    } else {
        tracing::warn!("Ignoring pasted text larger than {MAX_TEXT} bytes");
    }
    Ok(())
}
