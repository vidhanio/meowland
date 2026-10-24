//! Wayland client acceptance and compositor event-loop ownership.

use std::{
    io,
    sync::{Arc, mpsc},
    time::Instant,
};

use calloop::{EventLoop, Interest, Mode, PostAction, channel, generic::Generic};
use meowland_core::scale::UiScale;
use smithay::{
    input::keyboard::XkbConfig,
    reexports::wayland_server::{
        Display, ListeningSocket,
        backend::{ClientData, ClientId, DisconnectReason},
    },
    wayland::compositor::CompositorClientState,
};

use super::{Command, Event, State};

const ACCEPTS_PER_TURN: usize = 32;

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}

    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

struct Runtime {
    display: Display<State>,
    event_loop: EventLoop<'static, State>,
    state: State,
}

impl Runtime {
    fn new(
        name: &str,
        rx: channel::Channel<Command>,
        events: tokio::sync::mpsc::UnboundedSender<Event>,
        scale: UiScale,
    ) -> io::Result<Self> {
        let display = Display::<State>::new().map_err(io::Error::other)?;
        let event_loop = EventLoop::<State>::try_new()?;
        let mut state = State::new(display.handle(), event_loop.handle(), events, scale);
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
            .insert_source(rx, |event, (), state| match event {
                channel::Event::Msg(command) => state.handle_command(command),
                channel::Event::Closed => state.shutdown = true,
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
        handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state| {
                    for _ in 0..ACCEPTS_PER_TURN {
                        let Some(stream) = listener.accept()? else {
                            break;
                        };
                        if let Err(error) = state
                            .display
                            .insert_client(stream, Arc::new(ClientState::default()))
                        {
                            tracing::debug!(%error, "Cannot register Wayland client");
                        }
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

    fn run(mut self) {
        while !self.state.shutdown {
            if let Err(error) = self
                .event_loop
                .dispatch(self.state.next_wakeup(Instant::now()), &mut self.state)
            {
                tracing::warn!(%error, "compositor readiness loop stopped");
                break;
            }
            if let Err(error) = self.display.dispatch_clients(&mut self.state) {
                tracing::warn!(%error, "Wayland client dispatch stopped");
                break;
            }
            self.state.refresh_popup_grab();
            self.state.expire_activation_tokens();
            self.state.frame_callbacks();
            self.state.dispatch_frames();
            if let Err(error) = self.display.flush_clients() {
                tracing::warn!(%error, "Wayland client flush stopped");
                break;
            }
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the compositor thread's channels are handed over once, when the thread starts"
)]
pub fn run(
    name: String,
    rx: channel::Channel<Command>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    ready: mpsc::Sender<io::Result<()>>,
    scale: UiScale,
) {
    match Runtime::new(&name, rx, events, scale) {
        Ok(runtime) => {
            if ready.send(Ok(())).is_ok() {
                runtime.run();
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
}
