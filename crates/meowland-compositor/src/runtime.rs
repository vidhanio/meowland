//! Wayland client acceptance and compositor event-loop ownership.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, generic::Generic};
use meowland_core::scale::UiScale;
use smithay::{
    input::keyboard::XkbConfig,
    reexports::wayland_server::{
        Display, ListeningSocket,
        backend::{ClientData, ClientId, DisconnectReason},
    },
    wayland::compositor::CompositorClientState,
};

use super::State;

#[derive(Default)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}

    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// A compositor and its event loop, driven on the calling thread.
#[derive(Debug)]
pub struct Runtime {
    display: Display<State>,
    event_loop: EventLoop<'static, State>,
    state: State,
}

impl Runtime {
    /// Bind a Wayland socket and initialize the renderer and seat.
    ///
    /// # Errors
    /// Returns an error if the display, socket, event sources or keymap cannot
    /// start.
    pub fn new(name: &str, scale: UiScale) -> io::Result<Self> {
        let display = Display::<State>::new().map_err(io::Error::other)?;
        let event_loop = EventLoop::<State>::try_new()?;
        let mut state = State::new(display.handle(), event_loop.handle(), scale);
        let keymap = XkbConfig {
            layout: "us",
            ..XkbConfig::default()
        };
        state.keyboard = Some(
            state
                .seat
                .add_keyboard(keymap, 600, 25)
                .map_err(io::Error::other)?,
        );
        let listener = ListeningSocket::bind(name).map_err(io::Error::other)?;
        let handle = event_loop.handle();
        handle
            .insert_source(
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
            )
            .map_err(io::Error::other)?;
        handle
            .insert_source(
                Generic::new(rustix::io::dup(&display)?, Interest::READ, Mode::Level),
                |_, _, _| Ok(PostAction::Continue),
            )
            .map_err(io::Error::other)?;
        Ok(Self {
            display,
            event_loop,
            state,
        })
    }

    /// Register host sockets, timers, signals or async tasks on the same loop.
    #[must_use]
    pub fn handle(&self) -> LoopHandle<'static, State> {
        self.event_loop.handle()
    }

    /// Access the compositor without a command channel or mirrored state.
    pub const fn compositor(&mut self) -> &mut State {
        &mut self.state
    }

    /// Dispatch one turn, respecting both host and compositor deadlines.
    ///
    /// # Errors
    /// Returns an error if readiness polling or Wayland dispatch fails.
    pub fn dispatch(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let timeout = timeout
            .into_iter()
            .chain(self.state.next_wakeup(Instant::now()))
            .min();
        self.event_loop.dispatch(timeout, &mut self.state)?;
        self.display.dispatch_clients(&mut self.state)?;
        self.state.refresh_popup_grab();
        self.state.expire_activation_tokens();
        self.state.frame_callbacks();
        self.state.dispatch_frames();
        self.display.flush_clients()
    }
}
