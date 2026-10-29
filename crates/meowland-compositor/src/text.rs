//! Committed text delivery through focused text-input clients or the keyboard
//! fallback.

use smithay::{
    backend::input::InputTime,
    input::{Seat, keyboard::xkb},
    reexports::{
        wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
            ContentHint, ContentPurpose,
        },
        wayland_server::DisplayHandle,
    },
    wayland::text_input::{TextInputActivation, TextInputManagerState, TextInputSeat},
};

use super::State;

pub(crate) fn manager(display: &DisplayHandle, seat: &Seat<State>) -> TextInputManagerState {
    let manager = TextInputManagerState::new::<State>(display);
    seat.text_input().set_compositor_input_method(true);
    manager
}

pub(crate) fn commit(state: &mut State, text: &str) {
    if !valid(text) {
        return;
    }
    let handle = state.seat.text_input();
    let mut committed = false;
    handle.with_active_text_input(|input, _| {
        input.commit_string(Some(text.to_owned()));
        committed = true;
    });
    if committed {
        handle.done(false);
        return;
    }
    handle.leave();
    handle.enter();
    fallback(state, text);
}

impl TextInputActivation for State {
    fn activated(&mut self, _content_type: Option<(ContentHint, ContentPurpose)>) {
        self.seat.text_input().done(false);
    }
}

fn valid(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= meowland_protocol::MAX_INPUT_TEXT
        && !text.chars().any(char::is_control)
}

fn fallback(state: &mut State, text: &str) {
    let keysyms: Vec<_> = text
        .chars()
        .map(|character| xkb::utf32_to_keysym(u32::from(character)))
        .collect();
    let Some(keyboard) = state.keyboard.clone() else {
        return;
    };
    for chunk in keysyms.chunks(200) {
        keyboard.inject_text_keysyms(state, chunk, InputTime::now());
    }
}
