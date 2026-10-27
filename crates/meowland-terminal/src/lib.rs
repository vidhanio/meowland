//! Terminal input, graphics, and mode management for a pane.

use std::path::Path;

use meowland_core::{Error, Result};
use meowland_protocol::{Show, asynchronous::Reader};

mod input_gate;
mod output;
mod presentation;
mod probe;
mod session;
mod terminal;

/// Attach this terminal to a pane until release or disconnection.
///
/// This invocation exclusively owns terminal input and raw mode. Terminal
/// presentation starts with the first frame, leaving normal signal handling
/// intact while waiting for a window.
///
/// # Errors
/// Returns an error if the terminal cannot be configured or the pane connection
/// fails. Server-initiated release restores the terminal and returns `Ok`.
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
