//! Readiness-driven Wayland selection exports and terminal-backed deliveries.
//!
//! The bridge owns no Wayland state. A host supplies the seat selection calls,
//! pane routing, and event-loop projection through [`Host`].

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    fmt, io, mem,
    os::fd::OwnedFd,
    sync::Arc,
};

use calloop::{
    Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use meowland_loop::{Handle, Sources};
use meowland_panes::PaneId;
use meowland_protocol::{
    ClipboardOfferId, ClipboardRequestId, OfferIntent, ServerToPane,
    clipboard::{
        ClipboardData, Deadline, Item, MAX_BYTES, MAX_TEXT, Selection as ClipboardSelection,
        TEXT_MIMES, TRANSFER_TIMEOUT, Transport, mime_types,
    },
};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use smithay::wayland::selection::{SelectionSource, SelectionTarget};

/// How the current Wayland selection is backed.
#[derive(Clone, Debug)]
pub enum Source {
    /// Representations held by the bridge itself.
    Local(Arc<ClipboardData>),
    /// An offer whose data the terminal pane provides on request.
    Terminal {
        /// Pane that made the offer.
        pane: PaneId,
        /// Offer identifier the pane assigned.
        offer: ClipboardOfferId,
    },
}

/// Most deliveries in progress at once.
const MAX_DELIVERIES: usize = 16;
/// Bytes written to a client per readiness event.
const WRITE_BUDGET: usize = 64 * 1024;
/// Bytes read from the selection owner per read call.
const READ_CHUNK: usize = 16 * 1024;
/// Read calls per readiness event.
const READS_PER_EVENT: usize = 4;

/// Representations copied so far from the selection owner for one pane.
struct Export {
    pane: PaneId,
    mimes: VecDeque<String>,
    data: ClipboardData,
    bytes: usize,
    /// Aggregate bytes this transport can use, from [`export_limit`].
    limit: usize,
    deadline: Deadline,
}

/// Progress of the export for one seat selection.
enum Phase<H> {
    Idle,
    /// Waiting for Smithay to finish replacing the seat selection.
    Scheduled {
        export: Export,
        timer: Sources<H>,
    },
    /// Asking the selection owner for a representation. The export lives on
    /// the requesting stack frame; this marks whose request a re-entrant
    /// detach cancels.
    Requesting {
        pane: PaneId,
    },
    /// Reading one representation from the owner's pipe.
    Reading {
        export: Export,
        sources: Sources<H>,
    },
}

/// One seat selection's export, versioned so callbacks of a superseded
/// export recognize they are stale.
struct Slot<H> {
    epoch: u64,
    phase: Phase<H>,
}

impl<H> Slot<H> {
    const fn new() -> Self {
        Self {
            epoch: 0,
            phase: Phase::Idle,
        }
    }

    /// Supersede the current export, returning the new epoch.
    fn replace(&mut self, phase: Phase<H>) -> u64 {
        self.epoch += 1;
        self.phase = phase;
        self.epoch
    }

    const fn pane(&self) -> Option<PaneId> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Scheduled { export, .. } | Phase::Reading { export, .. } => Some(export.pane),
            Phase::Requesting { pane } => Some(*pane),
        }
    }

    /// Advance an export still at `epoch`, reporting whether it was current.
    fn advance(&mut self, epoch: u64, phase: Phase<H>) -> bool {
        if self.epoch != epoch {
            return false;
        }
        self.phase = phase;
        true
    }

    fn take_scheduled(&mut self, epoch: u64) -> Option<(Export, Sources<H>)> {
        match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Scheduled { export, timer } if self.epoch == epoch => Some((export, timer)),
            phase => {
                self.phase = phase;
                None
            }
        }
    }

    fn take_reading(&mut self, epoch: u64) -> Option<(Export, Sources<H>)> {
        match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Reading { export, sources } if self.epoch == epoch => Some((export, sources)),
            phase => {
                self.phase = phase;
                None
            }
        }
    }
}

enum DeliveryPhase {
    AwaitingTerminal { pane: PaneId, fd: OwnedFd },
    Writing,
}

struct Delivery<H> {
    phase: DeliveryPhase,
    deadline: Deadline,
    sources: Sources<H>,
}

const fn target_index(target: SelectionTarget) -> usize {
    match target {
        SelectionTarget::Clipboard => 0,
        SelectionTarget::Primary => 1,
    }
}

const fn wire_selection(target: SelectionTarget) -> ClipboardSelection {
    match target {
        SelectionTarget::Clipboard => ClipboardSelection::Clipboard,
        SelectionTarget::Primary => ClipboardSelection::Primary,
    }
}

const fn wayland_selection(selection: ClipboardSelection) -> SelectionTarget {
    match selection {
        ClipboardSelection::Clipboard => SelectionTarget::Clipboard,
        ClipboardSelection::Primary => SelectionTarget::Primary,
    }
}

/// Plain-text representations exported to every pane, most useful first.
///
/// Text leads because it is what a terminal pastes, and because legacy OSC 52
/// panes can paste nothing else.
const TEXT_EXPORT_MIMES: [&str; 5] = [
    TEXT_MIMES[0],
    TEXT_MIMES[1],
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

/// Representations exported only to MIME-aware Kitty panes.
///
/// `text/uri-list` carries file-manager copies, which are text and cheaper to
/// read than the image fallback that follows them.
const RICH_EXPORT_MIMES: [&str; 3] = ["text/html", "text/uri-list", "image/png"];

/// Aggregate bytes an export may copy from the application.
///
/// OSC 52 panes paste at most [`MAX_TEXT`] bytes of one text representation, so
/// reading more would only transfer data the pane discards.
const fn export_limit(transport: Transport) -> usize {
    match transport {
        Transport::Kitty => MAX_BYTES,
        Transport::Osc52 => MAX_TEXT,
    }
}

/// Select the representations to export over a pane's clipboard transport.
///
/// Any other type an application advertises is skipped rather than transferred:
/// copying an image in a browser would otherwise pull every format the
/// application can produce. OSC 52 carries a single text representation, so
/// asking for more only copies data the pane has to discard.
fn export_types(advertised: impl IntoIterator<Item = String>, transport: Transport) -> Vec<String> {
    let offered = mime_types(advertised);
    let usable = |mime: &&str| offered.iter().any(|offered| offered == mime);
    let text = TEXT_EXPORT_MIMES.into_iter().filter(usable);
    match transport {
        Transport::Kitty => text
            .chain(RICH_EXPORT_MIMES.into_iter().filter(usable))
            .map(str::to_owned)
            .collect(),
        Transport::Osc52 => text.take(1).map(str::to_owned).collect(),
    }
}

/// Host-owned selection exports, deliveries, and pending transfer state.
pub struct Bridge<H> {
    slots: [Slot<H>; 2],
    deliveries: HashMap<ClipboardRequestId, Delivery<H>>,
    next_request: u64,
}

impl<H> Default for Bridge<H> {
    fn default() -> Self {
        Self {
            slots: [Slot::new(), Slot::new()],
            deliveries: HashMap::new(),
            next_request: 0,
        }
    }
}

impl<H> fmt::Debug for Bridge<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bridge")
            .field(
                "exports",
                &self
                    .slots
                    .iter()
                    .filter(|slot| !matches!(slot.phase, Phase::Idle))
                    .count(),
            )
            .field("deliveries", &self.deliveries.len())
            .finish_non_exhaustive()
    }
}

impl<H> Bridge<H> {
    fn request(&mut self) -> ClipboardRequestId {
        self.next_request += 1;
        ClipboardRequestId::from(self.next_request)
    }

    const fn slot(&mut self, target: SelectionTarget) -> &mut Slot<H> {
        &mut self.slots[target_index(target)]
    }

    fn cancel(&mut self, target: SelectionTarget) {
        self.slot(target).replace(Phase::Idle);
    }

    /// Forget every delivery and export owned by a detached pane.
    fn detach(&mut self, pane: PaneId) {
        self.deliveries.retain(|_, delivery| {
            !matches!(delivery.phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
        });
        for slot in &mut self.slots {
            if slot.pane() == Some(pane) {
                slot.replace(Phase::Idle);
            }
        }
    }
}

/// Compositor operations the selection bridge needs from its host.
pub trait Host: Sized + 'static {
    /// Event-loop projection for selection timers and transfers.
    fn handle(&self) -> &Handle<Self>;
    /// Host-owned bridge state.
    fn bridge(&mut self) -> &mut Bridge<Self>;
    /// Send a protocol message to a pane.
    fn emit(&mut self, pane: PaneId, message: ServerToPane);
    /// Pane that should own the focused window's selection.
    fn selection_pane(&self) -> Option<PaneId>;
    /// Clipboard transport of an attached pane.
    fn pane_transport(&self, pane: PaneId) -> Option<Transport>;
    /// Whether a pane is attached.
    fn contains_pane(&self, pane: PaneId) -> bool;
    /// Whether a pane currently shows a mapped window, which a paste can
    /// focus and type into.
    fn pane_has_window(&self, pane: PaneId) -> bool;
    /// Ask the client that owns the selection for one representation.
    ///
    /// # Errors
    /// Returns an error when the seat cannot request the selection.
    fn request_selection(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) -> io::Result<()>;
    /// Offer representations to the seat selection.
    fn set_selection(&mut self, target: SelectionTarget, mimes: Vec<String>, source: Source);
    /// Pane whose offer currently backs the seat selection.
    fn selection_owner(&self, target: SelectionTarget) -> Option<PaneId>;
    /// Withdraw the seat selection.
    fn clear_selection(&mut self, target: SelectionTarget);
    /// Run the terminal paste shortcut in a pane showing a mapped window.
    fn paste(&mut self, pane: PaneId);
}

/// Handle a changed seat selection from the host's `new_selection` callback.
///
/// Smithay invokes this before it updates the seat selection, so exports are
/// requested from an immediate timer rather than synchronously.
///
/// Only a cleared selection clears the pane's clipboard. An export that
/// yields no representation, because none is supported or every transfer
/// fails, leaves the pane's clipboard untouched.
pub fn selection<H: Host>(host: &mut H, target: SelectionTarget, source: Option<SelectionSource>) {
    host.bridge().cancel(target);
    let Some(pane) = host.selection_pane() else {
        return;
    };
    let Some(source) = source else {
        host.emit(
            pane,
            ServerToPane::ClipboardWrite {
                selection: wire_selection(target),
                data: ClipboardData::default(),
            },
        );
        return;
    };
    let transport = host.pane_transport(pane).unwrap_or(Transport::Kitty);
    let export = Export {
        pane,
        mimes: export_types(source.mime_types(), transport).into(),
        data: ClipboardData::default(),
        bytes: 0,
        limit: export_limit(transport),
        deadline: Deadline::new(TRANSFER_TIMEOUT),
    };
    schedule_export(host, target, export);
}

/// Smithay updates the seat selection after returning from `new_selection`.
fn schedule_export<H: Host>(host: &mut H, target: SelectionTarget, export: Export) {
    let epoch = host.bridge().slot(target).replace(Phase::Idle);
    let mut timer = Sources::new(host.handle());
    match host
        .handle()
        .insert_source(Timer::immediate(), move |_, (), host| {
            if let Some((export, mut timer)) = host.bridge().slot(target).take_scheduled(epoch) {
                timer.timer = None;
                export_next(host, target, epoch, export);
            }
            TimeoutAction::Drop
        }) {
        Ok(token) => {
            timer.timer = Some(token);
            host.bridge()
                .slot(target)
                .advance(epoch, Phase::Scheduled { export, timer });
        }
        Err(error) => tracing::warn!(%error, "Cannot schedule clipboard export"),
    }
}

fn export_next<H: Host>(host: &mut H, target: SelectionTarget, epoch: u64, mut export: Export) {
    if export.deadline.expired() || !host.contains_pane(export.pane) {
        host.bridge().slot(target).advance(epoch, Phase::Idle);
        return;
    }
    let pane = export.pane;
    while let Some(mime) = export.mimes.pop_front() {
        if !host
            .bridge()
            .slot(target)
            .advance(epoch, Phase::Requesting { pane })
        {
            return;
        }
        match export_source(host, target, epoch, &export, mime.clone()) {
            Ok(Some(sources)) => {
                host.bridge()
                    .slot(target)
                    .advance(epoch, Phase::Reading { export, sources });
                return;
            }
            Ok(None) => {
                host.bridge().slot(target).advance(epoch, Phase::Idle);
                return;
            }
            Err(error) => tracing::warn!(%error, %mime, "Cannot read client clipboard"),
        }
    }
    if !host.bridge().slot(target).advance(epoch, Phase::Idle)
        || export.data.items.is_empty()
        || !host.contains_pane(pane)
    {
        return;
    }
    host.emit(
        pane,
        ServerToPane::ClipboardWrite {
            selection: wire_selection(target),
            data: export.data,
        },
    );
}

/// Request one representation, returning its registrations, or `None` if
/// the export was superseded or its pane detached during the request.
fn export_source<H: Host>(
    host: &mut H,
    target: SelectionTarget,
    epoch: u64,
    export: &Export,
    mime: String,
) -> io::Result<Option<Sources<H>>> {
    let (read, write) = rustix::pipe::pipe()?;
    nonblocking(&read)?;
    let mut sources = Sources::new(host.handle());
    let deadline = export.deadline;
    let limit = export.limit.saturating_sub(export.bytes);
    let mut bytes = Vec::new();
    let requested_mime = mime.clone();
    sources.io = Some(
        host.handle()
            .insert_source(
                Generic::new(read, Interest::READ, Mode::Level),
                move |_, fd, host| {
                    if host.bridge().slot(target).epoch != epoch {
                        return Ok(PostAction::Remove);
                    }
                    if deadline.expired() {
                        if let Some((_, mut sources)) =
                            host.bridge().slot(target).take_reading(epoch)
                        {
                            sources.io = None;
                        }
                        return Ok(PostAction::Remove);
                    }
                    let received = match receive(fd, &mut bytes, limit) {
                        Ok(false) => return Ok(PostAction::Continue),
                        Ok(true) => Some(mem::take(&mut bytes)),
                        Err(error) => {
                            tracing::warn!(%error, %mime, "Cannot transfer client clipboard");
                            None
                        }
                    };
                    let Some((mut export, mut sources)) =
                        host.bridge().slot(target).take_reading(epoch)
                    else {
                        return Ok(PostAction::Remove);
                    };
                    sources.io = None;
                    drop(sources);
                    if let Some(data) = received {
                        export.bytes += data.len();
                        export.data.items.push(Item {
                            mime: mime.clone(),
                            data,
                        });
                    }
                    export_next(host, target, epoch, export);
                    Ok(PostAction::Remove)
                },
            )
            .map_err(io::Error::other)?,
    );
    sources.timer = Some(
        host.handle()
            .insert_source(
                Timer::from_duration(export.deadline.remaining()),
                move |_, (), host| {
                    if let Some((_, mut sources)) = host.bridge().slot(target).take_reading(epoch) {
                        sources.timer = None;
                        tracing::warn!("Client clipboard export timed out");
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(io::Error::other)?,
    );
    let request = host.request_selection(target, requested_mime, write);
    if host.bridge().slot(target).epoch != epoch || !host.contains_pane(export.pane) {
        return Ok(None);
    }
    request?;
    Ok(Some(sources))
}

fn receive(fd: &impl std::os::fd::AsFd, bytes: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut chunk = [0; READ_CHUNK];
    for _ in 0..READS_PER_EVENT {
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

/// Deliver one representation of the seat selection to a client.
pub fn send<H: Host>(host: &mut H, fd: OwnedFd, mime: &str, source: &Source) {
    if host.bridge().deliveries.len() >= MAX_DELIVERIES {
        return;
    }
    let deadline = Deadline::new(TRANSFER_TIMEOUT);
    match source {
        Source::Local(data) => {
            if let Some(item) = data.items.iter().find(|item| item.mime == mime) {
                let request = host.bridge().request();
                write(host, request, fd, item.data.clone(), deadline);
            }
        }
        Source::Terminal { pane, offer } => {
            if !host.contains_pane(*pane) {
                return;
            }
            let request = host.bridge().request();
            let Ok(sources) = delivery_timer(host, request, deadline) else {
                return;
            };
            host.bridge().deliveries.insert(
                request,
                Delivery {
                    phase: DeliveryPhase::AwaitingTerminal { pane: *pane, fd },
                    deadline,
                    sources,
                },
            );
            host.emit(
                *pane,
                ServerToPane::ClipboardRead {
                    request,
                    offer: *offer,
                    mime: mime.to_owned(),
                    deadline,
                },
            );
        }
    }
}

fn delivery_timer<H: Host>(
    host: &H,
    request: ClipboardRequestId,
    deadline: Deadline,
) -> io::Result<Sources<H>> {
    let mut sources = Sources::new(host.handle());
    sources.timer = Some(
        host.handle()
            .insert_source(
                Timer::from_duration(deadline.remaining()),
                move |_, (), host| {
                    if let Some(mut delivery) = host.bridge().deliveries.remove(&request) {
                        delivery.sources.timer = None;
                        if let DeliveryPhase::AwaitingTerminal { pane, .. } = delivery.phase {
                            host.emit(pane, ServerToPane::ClipboardCancel { request });
                        }
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(io::Error::other)?,
    );
    Ok(sources)
}

fn nonblocking(fd: &OwnedFd) -> io::Result<()> {
    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    Ok(())
}

/// Write `data` to a client under the delivery identity `request`.
fn write<H: Host>(
    host: &mut H,
    request: ClipboardRequestId,
    fd: OwnedFd,
    data: Vec<u8>,
    deadline: Deadline,
) {
    if data.is_empty() || data.len() > MAX_BYTES || deadline.expired() {
        return;
    }
    let result = (|| -> io::Result<Sources<H>> {
        nonblocking(&fd)?;
        let mut sources = delivery_timer(host, request, deadline)?;
        let mut sent = 0;
        sources.io = Some(
            host.handle()
                .insert_source(
                    Generic::new(fd, Interest::WRITE, Mode::Level),
                    move |_, fd, host| {
                        if deadline.expired() {
                            if let Some(mut delivery) = host.bridge().deliveries.remove(&request) {
                                delivery.sources.io = None;
                            }
                            return Ok(PostAction::Remove);
                        }
                        let mut budget = WRITE_BUDGET;
                        while sent < data.len() && budget > 0 {
                            let end = (sent + budget).min(data.len());
                            match rustix::io::write(&*fd, &data[sent..end]) {
                                Ok(0) => break,
                                Ok(written) => {
                                    sent += written;
                                    budget -= written;
                                }
                                Err(Errno::AGAIN | Errno::INTR) => {
                                    return Ok(PostAction::Continue);
                                }
                                Err(_) => break,
                            }
                        }
                        if sent < data.len() && budget == 0 {
                            return Ok(PostAction::Continue);
                        }
                        if let Some(mut delivery) = host.bridge().deliveries.remove(&request) {
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
            host.bridge().deliveries.insert(
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

/// Complete a terminal-backed delivery with the pane's reply.
pub fn reply<H: Host>(
    host: &mut H,
    pane: PaneId,
    request: ClipboardRequestId,
    data: Option<Vec<u8>>,
) {
    let Entry::Occupied(entry) = host.bridge().deliveries.entry(request) else {
        return;
    };
    if !matches!(entry.get().phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
    {
        return;
    }
    let Delivery {
        phase,
        deadline,
        sources,
    } = entry.remove();
    drop(sources);
    if let DeliveryPhase::AwaitingTerminal { fd, .. } = phase
        && let Some(data) = data.filter(|data| data.len() <= MAX_BYTES)
    {
        write(host, request, fd, data, deadline);
    }
}

/// A selection a terminal pane wants to make available to Wayland clients.
#[derive(Debug)]
pub struct TerminalOffer {
    /// Which seat selection the offer replaces.
    pub selection: ClipboardSelection,
    /// Offer identifier the pane assigned.
    pub id: ClipboardOfferId,
    /// Representations the pane can provide.
    pub mimes: Vec<String>,
    /// What the pane expects to happen after offering.
    pub intent: OfferIntent,
}

/// Replace the seat selection with a terminal pane's offer.
pub fn offer<H: Host>(host: &mut H, pane: PaneId, offer: TerminalOffer) {
    let TerminalOffer {
        selection,
        id: offer,
        mimes,
        intent,
    } = offer;
    if !host.contains_pane(pane) {
        return;
    }
    let target = wayland_selection(selection);
    let mimes = mime_types(mimes);
    host.bridge().cancel(target);
    host.set_selection(target, mimes, Source::Terminal { pane, offer });
    if intent == OfferIntent::Paste
        && target == SelectionTarget::Clipboard
        && host.pane_has_window(pane)
    {
        host.paste(pane);
    }
}

/// Replace the seat clipboard with local text and paste it into a pane.
pub fn paste<H: Host>(host: &mut H, pane: PaneId, text: &str) {
    if text.len() > MAX_TEXT || !host.pane_has_window(pane) {
        return;
    }
    host.bridge().cancel(SelectionTarget::Clipboard);
    let data = Arc::new(ClipboardData::text(text));
    let mimes = data.items.iter().map(|item| item.mime.clone()).collect();
    host.set_selection(SelectionTarget::Clipboard, mimes, Source::Local(data));
    host.paste(pane);
}

/// Forget a detached pane's exports and deliveries, and withdraw every seat
/// selection its offers still back.
///
/// A newer selection that replaced the pane's offer is left in place.
pub fn detach<H: Host>(host: &mut H, pane: PaneId) {
    host.bridge().detach(pane);
    for target in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
        if host.selection_owner(target) == Some(pane) {
            host.clear_selection(target);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Write as _, time::Duration};

    use calloop::EventLoop;

    use super::*;

    type RequestAction = Box<dyn FnOnce(&mut FakeHost) -> io::Result<()>>;

    struct FakeHost {
        handle: Handle<Self>,
        bridge: Bridge<Self>,
        contains: bool,
        on_request: Option<RequestAction>,
        /// Bytes the selection owner writes per representation; `None` fails
        /// the request. Requests for representations not listed stay open.
        served: HashMap<String, Option<Vec<u8>>>,
        open: Vec<OwnedFd>,
        requests: Vec<String>,
        emitted: Vec<(PaneId, ServerToPane)>,
        /// Seat selections by [`target_index`].
        seat: [Option<Source>; 2],
        mapped: bool,
        pasted: Vec<PaneId>,
    }

    impl FakeHost {
        fn new() -> (EventLoop<'static, Self>, Self) {
            let event_loop = EventLoop::try_new().expect("event loop");
            let handle = Handle::new(event_loop.handle(), |host| host);
            let host = Self {
                handle,
                bridge: Bridge::default(),
                contains: true,
                on_request: None,
                served: HashMap::new(),
                open: Vec::new(),
                requests: Vec::new(),
                emitted: Vec::new(),
                seat: [None, None],
                mapped: true,
                pasted: Vec::new(),
            };
            (event_loop, host)
        }

        /// Start a clipboard export of `mimes` and request its first
        /// representation.
        fn start_export(&mut self, pane: PaneId, mimes: &[&str], limit: usize) {
            let target = SelectionTarget::Clipboard;
            let epoch = self.bridge.slot(target).replace(Phase::Idle);
            let export = Export {
                pane,
                mimes: mimes.iter().map(|mime| (*mime).to_owned()).collect(),
                data: ClipboardData::default(),
                bytes: 0,
                limit,
                deadline: Deadline::new(TRANSFER_TIMEOUT),
            };
            export_next(self, target, epoch, export);
        }

        fn exporting(&mut self) -> bool {
            !matches!(
                self.bridge.slot(SelectionTarget::Clipboard).phase,
                Phase::Idle
            )
        }

        fn serve(&mut self, mime: &str, data: Option<&[u8]>) {
            self.served
                .insert(mime.to_owned(), data.map(<[u8]>::to_vec));
        }

        /// Run transfers until the clipboard export finishes.
        fn settle(&mut self, event_loop: &mut EventLoop<'static, Self>) {
            for _ in 0..16 {
                if !self.exporting() {
                    return;
                }
                event_loop
                    .dispatch(Some(Duration::ZERO), self)
                    .expect("dispatch");
            }
            panic!("clipboard export did not finish");
        }

        /// Representations of every clipboard write sent to a pane.
        fn written(&self) -> Vec<Vec<(&str, &[u8])>> {
            self.emitted
                .iter()
                .filter_map(|(_, message)| match message {
                    ServerToPane::ClipboardWrite { data, .. } => Some(
                        data.items
                            .iter()
                            .map(|item| (item.mime.as_str(), item.data.as_slice()))
                            .collect(),
                    ),
                    _ => None,
                })
                .collect()
        }
    }

    impl Host for FakeHost {
        fn handle(&self) -> &Handle<Self> {
            &self.handle
        }

        fn bridge(&mut self) -> &mut Bridge<Self> {
            &mut self.bridge
        }

        fn emit(&mut self, pane: PaneId, message: ServerToPane) {
            self.emitted.push((pane, message));
        }

        fn selection_pane(&self) -> Option<PaneId> {
            Some(PaneId::from(1))
        }

        fn pane_transport(&self, _pane: PaneId) -> Option<Transport> {
            Some(Transport::Kitty)
        }

        fn contains_pane(&self, _pane: PaneId) -> bool {
            self.contains
        }

        fn pane_has_window(&self, _pane: PaneId) -> bool {
            self.mapped
        }

        fn request_selection(
            &mut self,
            _target: SelectionTarget,
            mime: String,
            fd: OwnedFd,
        ) -> io::Result<()> {
            self.requests.push(mime.clone());
            if let Some(action) = self.on_request.take() {
                return action(self);
            }
            match self.served.remove(&mime) {
                Some(Some(data)) => File::from(fd).write_all(&data),
                Some(None) => Err(io::Error::other("request failed")),
                None => {
                    self.open.push(fd);
                    Ok(())
                }
            }
        }

        fn set_selection(&mut self, target: SelectionTarget, _mimes: Vec<String>, source: Source) {
            self.seat[target_index(target)] = Some(source);
        }

        fn selection_owner(&self, target: SelectionTarget) -> Option<PaneId> {
            match self.seat[target_index(target)] {
                Some(Source::Terminal { pane, .. }) => Some(pane),
                Some(Source::Local(_)) | None => None,
            }
        }

        fn clear_selection(&mut self, target: SelectionTarget) {
            self.seat[target_index(target)] = None;
        }

        fn paste(&mut self, pane: PaneId) {
            self.pasted.push(pane);
        }
    }

    fn offer_both(host: &mut FakeHost, pane: PaneId) {
        for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
            offer(
                host,
                pane,
                TerminalOffer {
                    selection,
                    id: ClipboardOfferId::from(1),
                    mimes: vec!["text/plain".to_owned()],
                    intent: OfferIntent::Refresh,
                },
            );
        }
    }

    #[test]
    fn pastes_skip_panes_without_a_mapped_window() {
        let pane = PaneId::from(7);
        let paste_offer = || TerminalOffer {
            selection: ClipboardSelection::Clipboard,
            id: ClipboardOfferId::from(1),
            mimes: vec!["text/plain".to_owned()],
            intent: OfferIntent::Paste,
        };
        let (_event_loop, mut host) = FakeHost::new();

        host.mapped = false;
        offer(&mut host, pane, paste_offer());
        assert_eq!(host.selection_owner(SelectionTarget::Clipboard), Some(pane));
        paste(&mut host, pane, "text");
        assert_eq!(host.pasted, []);

        host.mapped = true;
        offer(&mut host, pane, paste_offer());
        paste(&mut host, pane, "text");
        assert_eq!(host.pasted, [pane, pane]);
    }

    #[test]
    fn detaching_a_pane_withdraws_only_the_offers_it_still_backs() {
        let (old, new) = (PaneId::from(7), PaneId::from(8));
        let (_event_loop, mut host) = FakeHost::new();

        offer_both(&mut host, old);
        detach(&mut host, old);
        assert!(host.seat.iter().all(Option::is_none));

        offer_both(&mut host, old);
        offer(
            &mut host,
            new,
            TerminalOffer {
                selection: ClipboardSelection::Primary,
                id: ClipboardOfferId::from(2),
                mimes: vec!["text/plain".to_owned()],
                intent: OfferIntent::Refresh,
            },
        );
        detach(&mut host, old);
        assert!(host.seat[target_index(SelectionTarget::Clipboard)].is_none());
        assert_eq!(host.selection_owner(SelectionTarget::Primary), Some(new));

        offer_both(&mut host, old);
        paste(&mut host, new, "local");
        detach(&mut host, old);
        assert!(matches!(
            host.seat[target_index(SelectionTarget::Clipboard)],
            Some(Source::Local(_))
        ));
        assert!(host.seat[target_index(SelectionTarget::Primary)].is_none());
    }

    #[test]
    fn reentrant_requests_discard_in_flight_export() {
        let pane = PaneId::from(7);
        let target = SelectionTarget::Clipboard;
        let (_event_loop, mut host) = FakeHost::new();

        host.on_request = Some(Box::new(move |host| {
            host.bridge.detach(pane);
            Ok(())
        }));
        host.start_export(pane, &["text/plain", "text/html"], MAX_BYTES);
        assert_eq!(host.requests.len(), 1);
        assert!(!host.exporting());
        assert!(host.emitted.is_empty());

        host.requests.clear();
        host.on_request = Some(Box::new(move |host| {
            host.bridge.cancel(target);
            Err(io::Error::other("request failed"))
        }));
        host.start_export(pane, &["text/plain", "text/html"], MAX_BYTES);
        assert_eq!(host.requests.len(), 1);
        assert!(!host.exporting());

        host.requests.clear();
        host.on_request = Some(Box::new(|_| Err(io::Error::other("request failed"))));
        host.start_export(pane, &["text/plain", "text/html"], MAX_BYTES);
        assert_eq!(
            host.requests.iter().map(String::as_str).collect::<Vec<_>>(),
            ["text/plain", "text/html"]
        );
        assert!(matches!(
            host.bridge.slot(target).phase,
            Phase::Reading { .. }
        ));
        assert!(host.emitted.is_empty());
    }

    #[test]
    fn unusable_exports_leave_the_terminal_clipboard_untouched() {
        let pane = PaneId::from(7);
        let (mut event_loop, mut host) = FakeHost::new();

        host.start_export(pane, &[], MAX_BYTES);
        host.settle(&mut event_loop);
        assert!(host.emitted.is_empty());

        host.serve("text/plain", None);
        host.serve("text/html", None);
        host.start_export(pane, &["text/plain", "text/html"], MAX_BYTES);
        host.settle(&mut event_loop);
        assert_eq!(host.requests, ["text/plain", "text/html"]);
        assert!(host.emitted.is_empty());
    }

    #[test]
    fn oversized_representations_keep_the_others() {
        let pane = PaneId::from(7);
        let (mut event_loop, mut host) = FakeHost::new();

        host.serve("text/plain", Some(b"hello"));
        host.serve("text/html", Some(b"<b>hello</b>"));
        host.start_export(pane, &["text/plain", "text/html"], 8);
        host.settle(&mut event_loop);
        assert_eq!(host.written(), [[("text/plain", &b"hello"[..])]]);

        host.emitted.clear();
        host.serve("text/html", Some(b"<b>hello</b>"));
        host.serve("text/plain", Some(b"hello"));
        host.start_export(pane, &["text/html", "text/plain"], 8);
        host.settle(&mut event_loop);
        assert_eq!(host.written(), [[("text/plain", &b"hello"[..])]]);
    }

    #[test]
    fn cleared_selections_and_empty_representations_reach_the_terminal() {
        let pane = PaneId::from(7);
        let target = SelectionTarget::Clipboard;
        let (mut event_loop, mut host) = FakeHost::new();

        selection(&mut host, target, None);
        assert_eq!(host.written(), [Vec::new()]);

        host.emitted.clear();
        host.serve("text/plain", Some(b""));
        host.start_export(pane, &["text/plain"], MAX_BYTES);
        host.settle(&mut event_loop);
        assert_eq!(host.written(), [[("text/plain", &b""[..])]]);
    }
}
