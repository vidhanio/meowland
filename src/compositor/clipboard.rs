//! Wayland selection offers, lazy terminal reads, and bounded copy exports.

use std::{
    collections::{HashMap, VecDeque},
    os::fd::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use smithay::wayland::selection::{
    SelectionSource,
    data_device::{request_data_device_client_selection, set_data_device_selection},
};

use super::{Event, State, input::paste_action};
use crate::clipboard::{ClipboardData, Item, MAX_BYTES, Transfer, mime_types};

#[derive(Clone, Debug)]
pub(super) enum Source {
    Local(Arc<ClipboardData>),
    Terminal { pane: u64, offer: u64 },
}

struct Export {
    pane: u64,
    mimes: VecDeque<String>,
    transfer: Option<(String, Transfer)>,
    data: ClipboardData,
    bytes: usize,
}

struct PendingRead {
    pane: u64,
    fd: OwnedFd,
    deadline: Instant,
}

struct PendingWrite {
    fd: OwnedFd,
    data: Arc<[u8]>,
    sent: usize,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct Bridge {
    export: Option<Export>,
    reads: HashMap<u64, PendingRead>,
    writes: Vec<PendingWrite>,
    next: u64,
}

impl Bridge {
    pub(super) fn pending(&self) -> bool {
        self.export.is_some() || !self.reads.is_empty() || !self.writes.is_empty()
    }

    pub(super) fn detach(&mut self, pane: u64) {
        self.reads.retain(|_, read| read.pane != pane);
        if self
            .export
            .as_ref()
            .is_some_and(|export| export.pane == pane)
        {
            self.export = None;
        }
    }
}

pub(super) fn selection(state: &mut State, source: Option<SelectionSource>) {
    state.clipboard.export = None;
    let pane = state
        .cursor_pane
        .filter(|pane| {
            state
                .panes
                .get(pane)
                .is_some_and(|pane| Some(pane.window) == state.focused)
        })
        .or_else(|| {
            state
                .panes
                .iter()
                .filter(|(_, pane)| Some(pane.window) == state.focused)
                .map(|(id, _)| *id)
                .min()
        });
    let Some(pane) = pane else {
        return;
    };
    let Some(source) = source else {
        let _ = state.events.send(Event::ClipboardWrite {
            pane,
            data: ClipboardData::default(),
        });
        return;
    };
    state.clipboard.export = Some(Export {
        pane,
        mimes: mime_types(source.mime_types()).into(),
        transfer: None,
        data: ClipboardData::default(),
        bytes: 0,
    });
}

pub(super) fn send(state: &mut State, fd: OwnedFd, mime: &str, source: &Source) {
    match source {
        Source::Local(data) => {
            if let Some(item) = data.items.iter().find(|item| item.mime == mime) {
                write(state, fd, Arc::from(item.data.as_slice()));
            }
        }
        Source::Terminal { pane, offer } => {
            if !state.panes.contains_key(pane) || state.clipboard.reads.len() >= 16 {
                return;
            }
            state.clipboard.next += 1;
            let request = state.clipboard.next;
            state.clipboard.reads.insert(
                request,
                PendingRead {
                    pane: *pane,
                    fd,
                    deadline: Instant::now() + Duration::from_secs(65),
                },
            );
            let _ = state.events.send(Event::ClipboardRead {
                pane: *pane,
                request,
                offer: *offer,
                mime: mime.to_owned(),
            });
        }
    }
}

fn write(state: &mut State, fd: OwnedFd, data: Arc<[u8]>) {
    if state.clipboard.writes.len() >= 16 || data.len() > MAX_BYTES {
        return;
    }
    let Ok(flags) = fcntl_getfl(&fd) else {
        return;
    };
    if fcntl_setfl(&fd, flags | OFlags::NONBLOCK).is_err() {
        return;
    }
    state.clipboard.writes.push(PendingWrite {
        fd,
        data,
        sent: 0,
        deadline: Instant::now() + Duration::from_secs(75),
    });
}

pub(super) fn reply(state: &mut State, pane: u64, request: u64, data: Option<Vec<u8>>) {
    if state
        .clipboard
        .reads
        .get(&request)
        .is_none_or(|read| read.pane != pane)
    {
        return;
    }
    if let Some(read) = state.clipboard.reads.remove(&request)
        && let Some(data) = data.filter(|data| data.len() <= MAX_BYTES)
    {
        write(state, read.fd, data.into());
    }
}

pub(super) fn offer(state: &mut State, pane: u64, offer: u64, mimes: Vec<String>, paste: bool) {
    if !state.panes.contains_key(&pane) {
        return;
    }
    let mimes = mime_types(mimes);
    state.clipboard.export = None;
    set_data_device_selection(
        &state.display,
        &state.seat,
        mimes,
        Source::Terminal { pane, offer },
    );
    if paste {
        paste_action(state, pane);
    }
}

pub(super) fn paste(state: &mut State, pane: u64, text: &str) {
    if text.len() > crate::clipboard::MAX_TEXT
        || state.panes.get(&pane).is_none_or(|pane| pane.window == 0)
    {
        return;
    }
    state.clipboard.export = None;
    let data = Arc::new(ClipboardData::text(text));
    let mimes = data.items.iter().map(|item| item.mime.clone()).collect();
    set_data_device_selection(&state.display, &state.seat, mimes, Source::Local(data));
    paste_action(state, pane);
}

pub(super) fn poll(state: &mut State) {
    if let Some(mut export) = state.clipboard.export.take()
        && state.panes.contains_key(&export.pane)
    {
        if export.transfer.is_none()
            && let Some(mime) = export.mimes.pop_front()
        {
            match rustix::pipe::pipe() {
                Ok((read, write)) => {
                    if request_data_device_client_selection(&state.seat, mime.clone(), write)
                        .is_ok()
                    {
                        match Transfer::new(read) {
                            Ok(transfer) => export.transfer = Some((mime, transfer)),
                            Err(error) => tracing::warn!(%error, "Cannot read client clipboard"),
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "Cannot create clipboard pipe"),
            }
        }
        if let Some((mime, transfer)) = &mut export.transfer {
            match transfer.receive() {
                Ok(Some(data)) => {
                    export.bytes += data.len();
                    if export.bytes <= MAX_BYTES {
                        export.data.items.push(Item {
                            mime: mime.clone(),
                            data,
                        });
                    } else {
                        tracing::warn!("Clipboard export exceeds size limit");
                        export.mimes.clear();
                        export.data.items.clear();
                    }
                    export.transfer = None;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "Client clipboard transfer failed");
                    export.transfer = None;
                }
            }
        }
        if export.mimes.is_empty() && export.transfer.is_none() {
            if export.bytes <= MAX_BYTES {
                let _ = state.events.send(Event::ClipboardWrite {
                    pane: export.pane,
                    data: export.data,
                });
            }
        } else {
            state.clipboard.export = Some(export);
        }
    }
    let now = Instant::now();
    state.clipboard.reads.retain(|_, read| read.deadline > now);
    state.clipboard.writes.retain_mut(|write| {
        if now >= write.deadline {
            return false;
        }
        let end = (write.sent + 64 * 1024).min(write.data.len());
        match rustix::io::write(&write.fd, &write.data[write.sent..end]) {
            Ok(0) => false,
            Ok(sent) => {
                write.sent += sent;
                write.sent < write.data.len()
            }
            Err(Errno::AGAIN | Errno::INTR) => true,
            Err(_) => false,
        }
    });
}
