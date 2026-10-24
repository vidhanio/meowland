//! Wayland client acceptance and compositor event-loop ownership.

use std::{
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

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}

    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the compositor thread's channels are handed over once, when the thread starts"
)]
pub fn run(
    name: String,
    rx: channel::Channel<Command>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    ready: mpsc::Sender<std::io::Result<()>>,
    scale: UiScale,
) {
    let Ok(mut display) = Display::<State>::new() else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not create Wayland display",
        )));
        return;
    };
    let mut event_loop = match EventLoop::<State>::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(std::io::Error::other(error)));
            return;
        }
    };
    let mut state = State::new(display.handle(), event_loop.handle(), events, scale);
    let keymap = XkbConfig {
        layout: "us",
        ..XkbConfig::default()
    };
    let Ok(keyboard) = state.seat.add_keyboard(keymap, 600, 25) else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not compile the keyboard keymap",
        )));
        return;
    };
    state.keyboard = Some(keyboard);
    let Ok(listener) = ListeningSocket::bind(name) else {
        let _ = ready.send(Err(std::io::Error::other("could not bind Wayland socket")));
        return;
    };
    let setup = (|| -> std::io::Result<()> {
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |event, (), state| match event {
                channel::Event::Msg(command) => state.handle_command(command),
                channel::Event::Closed => state.shutdown = true,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state| {
                    while let Some(stream) = listener.accept()? {
                        let _ = state
                            .display
                            .insert_client(stream, Arc::new(ClientState::default()));
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(std::io::Error::other)?;
        handle
            .insert_source(
                Generic::new(rustix::io::dup(&display)?, Interest::READ, Mode::Level),
                |_, _, _| Ok(PostAction::Continue),
            )
            .map_err(std::io::Error::other)?;
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));
    while !state.shutdown {
        if let Err(error) = event_loop.dispatch(state.next_wakeup(Instant::now()), &mut state) {
            tracing::warn!(%error, "compositor readiness loop stopped");
            break;
        }
        let _ = display.dispatch_clients(&mut state);
        state.refresh_popup_grab();
        state.expire_activation_tokens();
        state.frame_callbacks();
        state.dispatch_frames();
        let _ = display.flush_clients();
    }
}
