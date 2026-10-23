//! Readiness-driven Wayland selection exports and terminal-backed deliveries.

use std::{
    collections::{HashMap, VecDeque},
    io,
    os::fd::OwnedFd,
    sync::Arc,
};

use calloop::{
    Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use meowland_protocol::clipboard::{
    ClipboardData, Deadline, Item, MAX_BYTES, TRANSFER_TIMEOUT, mime_types,
};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use smithay::wayland::selection::{
    SelectionSource,
    data_device::{request_data_device_client_selection, set_data_device_selection},
};

use super::{Event, PaneId, State, input::paste_action, sources::Sources};

#[derive(Clone, Debug)]
pub enum Source {
    Local(Arc<ClipboardData>),
    Terminal { pane: PaneId, offer: u64 },
}

struct Export {
    id: u64,
    pane: PaneId,
    mimes: VecDeque<String>,
    active: Option<Sources>,
    data: ClipboardData,
    bytes: usize,
    deadline: Deadline,
}

enum DeliveryPhase {
    AwaitingTerminal { pane: PaneId, fd: OwnedFd },
    Writing,
}

struct Delivery {
    phase: DeliveryPhase,
    deadline: Deadline,
    sources: Sources,
}

#[derive(Default)]
pub struct Bridge {
    export: Option<Export>,
    deliveries: HashMap<u64, Delivery>,
    next: u64,
}

impl Bridge {
    const fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    pub(super) fn detach(&mut self, pane: PaneId) {
        self.deliveries.retain(|_, delivery| {
            !matches!(delivery.phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
        });
        if self
            .export
            .as_ref()
            .is_some_and(|export| export.pane == pane)
        {
            self.export = None;
        }
    }
}

pub fn selection(state: &mut State, source: Option<SelectionSource>) {
    state.clipboard.export = None;
    let pane = state
        .cursor_pane
        .filter(|pane| {
            state.attachments.panes.get(pane).is_some_and(|pane| {
                pane.window().is_some() && pane.window() == state.catalog.focused()
            })
        })
        .or_else(|| {
            state
                .attachments
                .panes
                .iter()
                .filter(|(_, pane)| {
                    pane.window().is_some() && pane.window() == state.catalog.focused()
                })
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
    let id = state.clipboard.id();
    state.clipboard.export = Some(Export {
        id,
        pane,
        mimes: mime_types(source.mime_types()).into(),
        active: None,
        data: ClipboardData::default(),
        bytes: 0,
        deadline: Deadline::new(TRANSFER_TIMEOUT),
    });
    schedule_export(state, id);
}

/// Smithay updates the seat selection after returning from `new_selection`.
fn schedule_export(state: &mut State, id: u64) {
    let mut sources = Sources::new(&state.handle);
    match state
        .handle
        .insert_source(Timer::immediate(), move |_, (), state| {
            if let Some(export) = &mut state.clipboard.export
                && export.id == id
            {
                if let Some(sources) = &mut export.active {
                    sources.timer = None;
                }
                export.active = None;
                export_next(state);
            }
            TimeoutAction::Drop
        }) {
        Ok(token) => {
            sources.timer = Some(token);
            state.clipboard.export.as_mut().unwrap().active = Some(sources);
        }
        Err(error) => {
            tracing::warn!(%error, "Cannot schedule clipboard export");
            state.clipboard.export = None;
        }
    }
}

fn export_next(state: &mut State) {
    let Some(mut export) = state.clipboard.export.take() else {
        return;
    };
    if export.deadline.expired() || !state.attachments.panes.contains_key(&export.pane) {
        return;
    }
    while let Some(mime) = export.mimes.pop_front() {
        let result = export_source(state, &export, mime.clone());
        match result {
            Ok(sources) => {
                export.active = Some(sources);
                state.clipboard.export = Some(export);
                return;
            }
            Err(error) => tracing::warn!(%error, %mime, "Cannot read client clipboard"),
        }
    }
    let _ = state.events.send(Event::ClipboardWrite {
        pane: export.pane,
        data: export.data,
    });
}

fn export_source(state: &State, export: &Export, mime: String) -> io::Result<Sources> {
    let (read, write) = rustix::pipe::pipe()?;
    nonblocking(&read)?;
    let mut sources = Sources::new(&state.handle);
    let id = export.id;
    let deadline = export.deadline;
    let limit = MAX_BYTES - export.bytes;
    let mut bytes = Vec::new();
    let requested_mime = mime.clone();
    sources.io = Some(
        state
            .handle
            .insert_source(
                Generic::new(read, Interest::READ, Mode::Level),
                move |_, fd, state| {
                    if state
                        .clipboard
                        .export
                        .as_ref()
                        .is_none_or(|export| export.id != id)
                    {
                        return Ok(PostAction::Remove);
                    }
                    if deadline.expired() {
                        let mut export = state.clipboard.export.take().unwrap();
                        if let Some(sources) = &mut export.active {
                            sources.io = None;
                        }
                        return Ok(PostAction::Remove);
                    }
                    let result = receive(fd, &mut bytes, limit);
                    if matches!(result, Ok(false)) {
                        return Ok(PostAction::Continue);
                    }
                    let export = state.clipboard.export.as_mut().unwrap();
                    if let Some(sources) = &mut export.active {
                        sources.io = None;
                    }
                    export.active = None;
                    match result {
                        Ok(true) => {
                            export.bytes += bytes.len();
                            export.data.items.push(Item {
                                mime: mime.clone(),
                                data: std::mem::take(&mut bytes),
                            });
                            export_next(state);
                        }
                        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                            tracing::warn!(%error, "Clipboard export exceeds size limit");
                            state.clipboard.export = None;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Client clipboard transfer failed");
                            export_next(state);
                        }
                        Ok(false) => unreachable!(),
                    }
                    Ok(PostAction::Remove)
                },
            )
            .map_err(io::Error::other)?,
    );
    sources.timer = Some(
        state
            .handle
            .insert_source(
                Timer::from_duration(export.deadline.remaining()),
                move |_, (), state| {
                    if state
                        .clipboard
                        .export
                        .as_ref()
                        .is_some_and(|export| export.id == id)
                    {
                        let mut export = state.clipboard.export.take().unwrap();
                        if let Some(sources) = &mut export.active {
                            sources.timer = None;
                        }
                        tracing::warn!("Client clipboard export timed out");
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| io::Error::other(error.error))?,
    );
    request_data_device_client_selection(&state.seat, requested_mime, write)
        .map_err(io::Error::other)?;
    Ok(sources)
}

fn receive(fd: &impl std::os::fd::AsFd, bytes: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut chunk = [0; 16 * 1024];
    for _ in 0..4 {
        match rustix::io::read(fd, &mut chunk) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                if bytes.len() + read > limit {
                    return Err(io::ErrorKind::FileTooLarge.into());
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Err(Errno::AGAIN) => return Ok(false),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

pub fn send(state: &mut State, fd: OwnedFd, mime: &str, source: &Source) {
    if state.clipboard.deliveries.len() >= 16 {
        return;
    }
    let deadline = Deadline::new(TRANSFER_TIMEOUT);
    match source {
        Source::Local(data) => {
            if let Some(item) = data.items.iter().find(|item| item.mime == mime) {
                write(state, fd, Arc::from(item.data.as_slice()), deadline);
            }
        }
        Source::Terminal { pane, offer } => {
            if !state.attachments.panes.contains_key(pane) {
                return;
            }
            let request = state.clipboard.id();
            let Ok(sources) = delivery_timer(state, request, deadline) else {
                return;
            };
            state.clipboard.deliveries.insert(
                request,
                Delivery {
                    phase: DeliveryPhase::AwaitingTerminal { pane: *pane, fd },
                    deadline,
                    sources,
                },
            );
            if state
                .events
                .send(Event::ClipboardRead {
                    pane: *pane,
                    request,
                    offer: *offer,
                    mime: mime.to_owned(),
                    deadline,
                })
                .is_err()
            {
                state.clipboard.deliveries.remove(&request);
            }
        }
    }
}

fn delivery_timer(state: &State, request: u64, deadline: Deadline) -> io::Result<Sources> {
    let mut sources = Sources::new(&state.handle);
    sources.timer = Some(
        state
            .handle
            .insert_source(
                Timer::from_duration(deadline.remaining()),
                move |_, (), state| {
                    if let Some(mut delivery) = state.clipboard.deliveries.remove(&request) {
                        delivery.sources.timer = None;
                        if let DeliveryPhase::AwaitingTerminal { pane, .. } = delivery.phase {
                            let _ = state.events.send(Event::ClipboardCancel { pane, request });
                        }
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| io::Error::other(error.error))?,
    );
    Ok(sources)
}

fn nonblocking(fd: &OwnedFd) -> io::Result<()> {
    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    Ok(())
}

fn write(state: &mut State, fd: OwnedFd, data: Arc<[u8]>, deadline: Deadline) {
    if data.is_empty() || data.len() > MAX_BYTES || deadline.expired() {
        return;
    }
    let request = state.clipboard.id();
    let result = (|| -> io::Result<Sources> {
        nonblocking(&fd)?;
        let mut sources = delivery_timer(state, request, deadline)?;
        let mut sent = 0;
        sources.io = Some(
            state
                .handle
                .insert_source(
                    Generic::new(fd, Interest::WRITE, Mode::Level),
                    move |_, fd, state| {
                        if deadline.expired() {
                            if let Some(mut delivery) = state.clipboard.deliveries.remove(&request)
                            {
                                delivery.sources.io = None;
                            }
                            return Ok(PostAction::Remove);
                        }
                        let mut budget = 64 * 1024;
                        while sent < data.len() && budget > 0 {
                            let end = (sent + budget).min(data.len());
                            match rustix::io::write(&*fd, &data[sent..end]) {
                                Ok(0) => break,
                                Ok(written) => {
                                    sent += written;
                                    budget -= written;
                                }
                                Err(Errno::AGAIN | Errno::INTR) => return Ok(PostAction::Continue),
                                Err(_) => break,
                            }
                        }
                        if sent < data.len() && budget == 0 {
                            return Ok(PostAction::Continue);
                        }
                        if let Some(mut delivery) = state.clipboard.deliveries.remove(&request) {
                            delivery.sources.io = None;
                        }
                        Ok(PostAction::Remove)
                    },
                )
                .map_err(io::Error::other)?,
        );
        Ok(sources)
    })();
    match result {
        Ok(sources) => {
            state.clipboard.deliveries.insert(
                request,
                Delivery {
                    phase: DeliveryPhase::Writing,
                    deadline,
                    sources,
                },
            );
        }
        Err(error) => tracing::warn!(%error, "Cannot write client clipboard"),
    }
}

pub fn reply(state: &mut State, pane: PaneId, request: u64, data: Option<Vec<u8>>) {
    if state.clipboard.deliveries.get(&request).is_none_or(|delivery| {
        !matches!(delivery.phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
    }) {
        return;
    }
    if let Some(delivery) = state.clipboard.deliveries.remove(&request) {
        let DeliveryPhase::AwaitingTerminal { fd, .. } = delivery.phase else {
            unreachable!();
        };
        drop(delivery.sources);
        if let Some(data) = data.filter(|data| data.len() <= MAX_BYTES) {
            write(state, fd, data.into(), delivery.deadline);
        }
    }
}

pub fn offer(state: &mut State, pane: PaneId, offer: u64, mimes: Vec<String>, paste: bool) {
    if !state.attachments.panes.contains_key(&pane) {
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

pub fn paste(state: &mut State, pane: PaneId, text: &str) {
    if text.len() > meowland_protocol::clipboard::MAX_TEXT
        || state
            .attachments
            .panes
            .get(&pane)
            .is_none_or(|pane| pane.window().is_none())
    {
        return;
    }
    state.clipboard.export = None;
    let data = Arc::new(ClipboardData::text(text));
    let mimes = data.items.iter().map(|item| item.mime.clone()).collect();
    set_data_device_selection(&state.display, &state.seat, mimes, Source::Local(data));
    paste_action(state, pane);
}
