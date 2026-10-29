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
        Display, ListeningSocket,
        backend::{ClientData, ClientId, DisconnectReason},
    },
    wayland::compositor::CompositorClientState,
};

use super::{CompositorHandle, Settings, State};

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
    display: Display<State>,
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
    ) -> io::Result<(Self, State)> {
        let display = Display::<State>::new().map_err(io::Error::other)?;
        let mut state = State::new(display.handle(), handle.clone(), settings);
        let keyboard = &settings.keyboard;
        let keymap = XkbConfig {
            layout: &keyboard.layout,
            variant: &keyboard.variant,
            options: (!keyboard.options.is_empty()).then(|| keyboard.options.clone()),
            ..XkbConfig::default()
        };
        state.keyboard = Some(
            state
                .seat
                .add_keyboard(
                    keymap,
                    keyboard.repeat_delay.milliseconds(),
                    keyboard.repeat_rate.hertz(),
                )
                .map_err(io::Error::other)?,
        );
        let listener = ListeningSocket::bind(name).map_err(io::Error::other)?;
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
    pub fn dispatch(&mut self, state: &mut State) -> io::Result<()> {
        self.display.dispatch_clients(state)?;
        state.refresh_popup_grab();
        state.expire_activation_tokens();
        state.frame_callbacks();
        state.dispatch_frames();
        self.display.flush_clients()
    }

    /// Time remaining until compositor work needs another dispatch turn.
    pub fn next_wakeup(state: &State) -> Option<Duration> {
        state.next_wakeup(Instant::now())
    }
}
