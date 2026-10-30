//! Terminal input, graphics, and mode management for a pane.

use std::path::Path;

use calloop::{Dispatcher, EventLoop, futures::executor};
use meowland_async::asynchronous::{connect, timeout};
use meowland_protocol::{Show, asynchronous::Reader};

mod error;
mod input_gate;
mod output;
mod presentation;
mod probe;
mod session;
mod terminal;

pub use error::{Error, Result};
#[cfg(feature = "fuzzing")]
pub use probe::fuzzing;

/// Attach this terminal to a pane until release or disconnection.
///
/// This invocation exclusively owns terminal input and raw mode. Terminal
/// presentation starts with the first frame, leaving normal signal handling
/// intact while waiting for a window.
///
/// # Errors
/// Returns an error if the terminal cannot be configured or the pane connection
/// fails. Server-initiated release restores the terminal and returns `Ok`.
pub fn attach(socket: &Path, show: Show) -> Result<()> {
    let mut event_loop = EventLoop::<()>::try_new().map_err(std::io::Error::from)?;
    let handle = event_loop.handle();
    let (executor, scheduler) = executor().map_err(std::io::Error::from)?;
    let (finished, result) = std::sync::mpsc::sync_channel(1);
    let executor = Dispatcher::new(executor, move |result, (), ()| {
        let _ = finished.send(result);
    });
    let token = handle
        .register_dispatcher(executor.clone())
        .map_err(std::io::Error::from)?;
    let io_handle = handle.clone();
    let socket = socket.to_owned();
    scheduler
        .schedule(async move {
            let handle = io_handle;
            let mut stream = timeout(
                &handle,
                meowland_protocol::REQUEST_TIMEOUT,
                connect(&handle, &socket),
            )
            .await
            .map_err(|source| Error::Connect {
                path: socket.clone(),
                source,
            })?;
            let writer = handle
                .adapt_io(stream.get_mut().try_clone()?)
                .map_err(std::io::Error::from)?;
            session::TerminalSession::new(Reader::new(stream), writer, show, handle)?
                .run()
                .await
        })
        .map_err(std::io::Error::other)?;
    let outcome = (|| loop {
        event_loop
            .dispatch(None, &mut ())
            .map_err(std::io::Error::from)?;
        if let Ok(result) = result.try_recv() {
            return result;
        }
    })();
    handle.remove(token);
    drop(executor);
    outcome
}
