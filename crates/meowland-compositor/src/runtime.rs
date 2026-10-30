//! Wayland dispatch on the host's event loop and calling thread.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{Interest, Mode, PostAction, generic::Generic};
use smithay::{
    input::keyboard::XkbConfig,
    reexports::wayland_server::{
        BindError, Display, ListeningSocket,
        backend::{ClientData, ClientId, DisconnectReason, InitError},
    },
    wayland::compositor::CompositorClientState,
};

use super::{Compositor, CompositorHandle, Settings};

#[derive(Default)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}

    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// Wayland display dispatch, sharing the host's loop and uniquely owned state.
#[derive(Debug)]
pub struct Runtime {
    display: Display<Compositor>,
}

impl Runtime {
    /// Bind a Wayland socket and register its sources on the host loop.
    ///
    /// # Errors
    /// Returns an error for display, socket, source, renderer, or keymap
    /// initialization failure.
    pub fn with_settings(
        name: &str,
        settings: &Settings,
        handle: &CompositorHandle,
    ) -> io::Result<(Self, Compositor)> {
        let display = Display::<Compositor>::new().map_err(init_error)?;
        let mut state = Compositor::new(display.handle(), handle.clone(), settings);
        let keyboard = &settings.keyboard;
        let keymap = XkbConfig {
            layout: &keyboard.layout,
            variant: &keyboard.variant,
            options: (!keyboard.options.is_empty()).then(|| keyboard.options.clone()),
            ..XkbConfig::default()
        };
        let keyboard_handle = state
            .input
            .seat
            .add_keyboard(
                keymap,
                keyboard.repeat_delay.milliseconds(),
                keyboard.repeat_rate.hertz(),
            )
            .map_err(io::Error::other)?;
        let keymap = super::keymap::Keymap::new(&keyboard_handle, &mut state);
        state.input.keymap = keymap;
        state.input.keyboard = Some(keyboard_handle);
        let listener = ListeningSocket::bind(name).map_err(bind_error)?;
        handle.insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state| {
                if let Some(stream) = listener.accept()?
                    && let Err(error) = state
                        .display
                        .insert_client(stream, Arc::new(ClientState::default()))
                {
                    tracing::debug!(%error, "Cannot register Wayland client");
                }
                Ok(PostAction::Continue)
            },
        )?;
        handle.insert_source(
            Generic::new(rustix::io::dup(&display)?, Interest::READ, Mode::Level),
            |_, _, _| Ok(PostAction::Continue),
        )?;
        Ok((Self { display }, state))
    }

    /// Dispatch and flush Wayland clients, callbacks, and ready frames.
    ///
    /// # Errors
    /// Returns an error if client dispatch or flushing fails.
    pub fn dispatch(&mut self, state: &mut Compositor) -> io::Result<()> {
        self.display.dispatch_clients(state)?;
        state.refresh_popup_grab();
        state.expire_activation_tokens();
        state.frame_callbacks();
        state.dispatch_frames();
        self.display.flush_clients()
    }

    /// Time remaining until compositor work needs another dispatch turn.
    pub fn next_wakeup(state: &Compositor) -> Option<Duration> {
        state.next_wakeup(Instant::now())
    }
}

/// Keep an upstream I/O cause out of a second [`io::Error`] layer.
fn init_error(error: InitError) -> io::Error {
    match error {
        InitError::Io(source) => source,
        other @ InitError::NoWaylandLib => io::Error::other(other),
    }
}

/// Keep an upstream I/O cause out of a second [`io::Error`] layer.
fn bind_error(error: BindError) -> io::Error {
    match error {
        BindError::Io(source) => source,
        other => io::Error::other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_chain(error: &io::Error) -> Vec<String> {
        let mut rendered = vec![error.to_string()];
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            rendered.push(cause.to_string());
            source = cause.source();
        }
        rendered
    }

    #[test]
    fn init_io_error_exposes_its_cause_once() {
        let cause = io::Error::from_raw_os_error(24);
        let message = cause.to_string();

        assert_eq!(rendered_chain(&init_error(InitError::Io(cause))), [message]);
    }

    #[test]
    fn bind_io_error_exposes_its_cause_once() {
        let cause = io::Error::from_raw_os_error(36);
        let message = cause.to_string();

        assert_eq!(rendered_chain(&bind_error(BindError::Io(cause))), [message]);
    }
}
