use std::{
    os::fd::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use smithay::wayland::selection::data_device::{
    request_data_device_client_selection, set_data_device_selection,
};

use super::{Event, State, input::pane_input};
use crate::{
    clipboard::{MAX_TEXT, TEXT_MIMES, Transfer},
    protocol::{Input, modifiers},
};

pub(super) struct PendingWrite {
    fd: OwnedFd,
    text: Arc<str>,
    sent: usize,
    deadline: Instant,
}

pub(super) fn send(state: &mut State, fd: OwnedFd, text: &Arc<str>) {
    if state.clipboard_writes.len() >= 16 {
        return;
    }
    let Ok(flags) = fcntl_getfl(&fd) else {
        return;
    };
    if fcntl_setfl(&fd, flags | OFlags::NONBLOCK).is_err() {
        return;
    }
    state.clipboard_writes.push(PendingWrite {
        fd,
        text: Arc::clone(text),
        sent: 0,
        deadline: Instant::now() + Duration::from_secs(2),
    });
}

pub(super) fn poll(state: &mut State) {
    if let Some((pane, mime)) = state.clipboard_request.take() {
        match rustix::pipe::pipe() {
            Ok((read, write)) => {
                if request_data_device_client_selection(&state.seat, mime, write).is_ok() {
                    match Transfer::new(read) {
                        Ok(transfer) => state.clipboard_read = Some((pane, transfer)),
                        Err(error) => tracing::warn!(%error, "Cannot read client clipboard"),
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "Cannot create clipboard pipe"),
        }
    }
    if let Some((pane, transfer)) = &mut state.clipboard_read {
        match transfer.receive() {
            Ok(Some(text)) => {
                if state.panes.contains_key(pane) {
                    let _ = state.events.send(Event::Clipboard { pane: *pane, text });
                }
                state.clipboard_read = None;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "Client clipboard transfer failed");
                state.clipboard_read = None;
            }
        }
    }
    state.clipboard_writes.retain_mut(|write| {
        if Instant::now() >= write.deadline {
            return false;
        }
        let end = (write.sent + 64 * 1024).min(write.text.len());
        match rustix::io::write(&write.fd, &write.text.as_bytes()[write.sent..end]) {
            Ok(0) => false,
            Ok(sent) => {
                write.sent += sent;
                write.sent < write.text.len()
            }
            Err(Errno::AGAIN | Errno::INTR) => true,
            Err(_) => false,
        }
    });
}

pub(super) fn paste(state: &mut State, pane: u64, text: String) {
    if text.len() > MAX_TEXT || state.panes.get(&pane).is_none_or(|pane| pane.window == 0) {
        return;
    }
    state.clipboard_request = None;
    state.clipboard_read = None;
    set_data_device_selection(
        &state.display,
        &state.seat,
        TEXT_MIMES.into_iter().map(str::to_owned).collect(),
        Arc::from(text),
    );
    for pressed in [true, false] {
        pane_input(
            state,
            pane,
            &Input::Key {
                code: 47,
                pressed,
                modifiers: modifiers::CONTROL,
            },
        );
    }
}
