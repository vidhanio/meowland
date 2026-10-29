//! Readiness-driven Wayland selection exports and terminal-backed deliveries.
//!
//! The bridge owns no Wayland state. A host supplies the seat selection calls,
//! pane routing, and event-loop projection through [`Host`].

use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
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
        TRANSFER_TIMEOUT, Transport, mime_types,
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

struct Export<H> {
    id: u64,
    target: SelectionTarget,
    pane: PaneId,
    mimes: VecDeque<String>,
    active: Option<Sources<H>>,
    data: ClipboardData,
    bytes: usize,
    /// Aggregate bytes this transport can use, from [`export_limit`].
    limit: usize,
    deadline: Deadline,
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
    "text/plain;charset=utf-8",
    "text/plain",
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

struct InFlight {
    pane: PaneId,
    generation: u64,
}

/// Host-owned selection exports, deliveries, and pending transfer state.
pub struct Bridge<H> {
    exports: [Option<Export<H>>; 2],
    in_flight: [Option<InFlight>; 2],
    deliveries: HashMap<ClipboardRequestId, Delivery<H>>,
    generations: [u64; 2],
    next: u64,
}

impl<H> Default for Bridge<H> {
    fn default() -> Self {
        Self {
            exports: [None, None],
            in_flight: [None, None],
            deliveries: HashMap::new(),
            generations: [0; 2],
            next: 0,
        }
    }
}

impl<H> fmt::Debug for Bridge<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bridge")
            .field("exports", &self.exports.iter().flatten().count())
            .field("deliveries", &self.deliveries.len())
            .finish_non_exhaustive()
    }
}

impl<H> Bridge<H> {
    const fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    const fn export(&self, target: SelectionTarget) -> Option<&Export<H>> {
        self.exports[target_index(target)].as_ref()
    }

    const fn export_mut(&mut self, target: SelectionTarget) -> Option<&mut Export<H>> {
        self.exports[target_index(target)].as_mut()
    }

    const fn take_export(&mut self, target: SelectionTarget) -> Option<Export<H>> {
        self.exports[target_index(target)].take()
    }

    const fn generation(&self, target: SelectionTarget) -> u64 {
        self.generations[target_index(target)]
    }

    fn set_export(&mut self, target: SelectionTarget, export: Option<Export<H>>) {
        self.exports[target_index(target)] = export;
        self.generations[target_index(target)] += 1;
    }

    /// Forget every delivery and export owned by a detached pane.
    pub fn detach(&mut self, pane: PaneId) {
        self.deliveries.retain(|_, delivery| {
            !matches!(delivery.phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
        });
        for ((export, in_flight), generation) in self
            .exports
            .iter_mut()
            .zip(&mut self.in_flight)
            .zip(&mut self.generations)
        {
            let detached = if export.as_ref().is_some_and(|export| export.pane == pane) {
                *export = None;
                true
            } else {
                false
            };
            let in_flight = in_flight.as_ref().is_some_and(|in_flight| {
                in_flight.pane == pane && in_flight.generation == *generation
            });
            if detached || in_flight {
                *generation += 1;
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
    /// Whether a pane currently shows a window.
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
    /// Run the terminal paste shortcut in a pane.
    fn paste(&mut self, pane: PaneId);
}

/// Handle a changed seat selection from the host's `new_selection` callback.
///
/// Smithay invokes this before it updates the seat selection, so exports are
/// requested from an immediate timer rather than synchronously.
pub fn selection<H: Host>(host: &mut H, target: SelectionTarget, source: Option<SelectionSource>) {
    host.bridge().set_export(target, None);
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
    let id = host.bridge().id();
    let transport = host.pane_transport(pane).unwrap_or(Transport::Kitty);
    host.bridge().set_export(
        target,
        Some(Export {
            id,
            target,
            pane,
            mimes: export_types(source.mime_types(), transport).into(),
            active: None,
            data: ClipboardData::default(),
            bytes: 0,
            limit: export_limit(transport),
            deadline: Deadline::new(TRANSFER_TIMEOUT),
        }),
    );
    schedule_export(host, target, id);
}

/// Smithay updates the seat selection after returning from `new_selection`.
fn schedule_export<H: Host>(host: &mut H, target: SelectionTarget, id: u64) {
    let mut sources = Sources::new(host.handle());
    match host
        .handle()
        .insert_source(Timer::immediate(), move |_, (), host| {
            if let Some(export) = host.bridge().export_mut(target)
                && export.id == id
            {
                if let Some(sources) = &mut export.active {
                    sources.timer = None;
                }
                export.active = None;
                export_next(host, target);
            }
            TimeoutAction::Drop
        }) {
        Ok(token) => {
            sources.timer = Some(token);
            if let Some(export) = host.bridge().export_mut(target) {
                export.active = Some(sources);
            }
        }
        Err(error) => {
            tracing::warn!(%error, "Cannot schedule clipboard export");
            host.bridge().set_export(target, None);
        }
    }
}

fn export_next<H: Host>(host: &mut H, target: SelectionTarget) {
    let Some(export) = host.bridge().take_export(target) else {
        return;
    };
    let generation = host.bridge().generation(target);
    host.bridge().in_flight[target_index(target)] = Some(InFlight {
        pane: export.pane,
        generation,
    });
    export_attempt(host, target, export, generation);
    host.bridge().in_flight[target_index(target)] = None;
}

fn export_attempt<H: Host>(
    host: &mut H,
    target: SelectionTarget,
    mut export: Export<H>,
    generation: u64,
) {
    if export.deadline.expired() || !host.contains_pane(export.pane) {
        return;
    }
    if host.bridge().generation(target) != generation {
        return;
    }
    while let Some(mime) = export.mimes.pop_front() {
        let result = export_source(host, &export, mime.clone(), generation);
        match result {
            Ok(Some(sources)) => {
                export.active = Some(sources);
                host.bridge().set_export(target, Some(export));
                return;
            }
            Ok(None) => return,
            Err(error) => tracing::warn!(%error, %mime, "Cannot read client clipboard"),
        }
    }
    if host.bridge().generation(target) != generation || !host.contains_pane(export.pane) {
        return;
    }
    host.emit(
        export.pane,
        ServerToPane::ClipboardWrite {
            selection: wire_selection(target),
            data: export.data,
        },
    );
}

fn export_source<H: Host>(
    host: &mut H,
    export: &Export<H>,
    mime: String,
    generation: u64,
) -> io::Result<Option<Sources<H>>> {
    let (read, write) = rustix::pipe::pipe()?;
    nonblocking(&read)?;
    let mut sources = Sources::new(host.handle());
    let id = export.id;
    let target = export.target;
    let deadline = export.deadline;
    let limit = export.limit.saturating_sub(export.bytes);
    let mut bytes = Vec::new();
    let requested_mime = mime.clone();
    sources.io = Some(
        host.handle()
            .insert_source(
                Generic::new(read, Interest::READ, Mode::Level),
                move |_, fd, host| {
                    if host
                        .bridge()
                        .export(target)
                        .is_none_or(|export| export.id != id)
                    {
                        return Ok(PostAction::Remove);
                    }
                    if deadline.expired() {
                        if let Some(mut export) = host.bridge().take_export(target)
                            && let Some(sources) = &mut export.active
                        {
                            sources.io = None;
                        }
                        return Ok(PostAction::Remove);
                    }
                    let result = receive(fd, &mut bytes, limit);
                    if matches!(result, Ok(false)) {
                        return Ok(PostAction::Continue);
                    }
                    let Some(export) = host.bridge().export_mut(target) else {
                        return Ok(PostAction::Remove);
                    };
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
                            export_next(host, target);
                        }
                        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                            tracing::warn!(%error, "Clipboard export exceeds size limit");
                            host.bridge().set_export(target, None);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Client clipboard transfer failed");
                            export_next(host, target);
                        }
                        Ok(false) => unreachable!(),
                    }
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
                    if let Some(mut export) = host.bridge().exports[target_index(target)]
                        .take_if(|export| export.id == id)
                    {
                        if let Some(sources) = &mut export.active {
                            sources.timer = None;
                        }
                        tracing::warn!("Client clipboard export timed out");
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(io::Error::other)?,
    );
    let request = host.request_selection(target, requested_mime, write);
    if host.bridge().generation(target) != generation || !host.contains_pane(export.pane) {
        return Ok(None);
    }
    request?;
    Ok(Some(sources))
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

/// Deliver one representation of the seat selection to a client.
pub fn send<H: Host>(host: &mut H, fd: OwnedFd, mime: &str, source: &Source) {
    if host.bridge().deliveries.len() >= 16 {
        return;
    }
    let deadline = Deadline::new(TRANSFER_TIMEOUT);
    match source {
        Source::Local(data) => {
            if let Some(item) = data.items.iter().find(|item| item.mime == mime) {
                write(host, fd, Arc::from(item.data.as_slice()), deadline);
            }
        }
        Source::Terminal { pane, offer } => {
            if !host.contains_pane(*pane) {
                return;
            }
            let request = ClipboardRequestId::from(host.bridge().id());
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

fn write<H: Host>(host: &mut H, fd: OwnedFd, data: Arc<[u8]>, deadline: Deadline) {
    if data.is_empty() || data.len() > MAX_BYTES || deadline.expired() {
        return;
    }
    let request = ClipboardRequestId::from(host.bridge().id());
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
                        let mut budget = 64 * 1024;
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
    if host
        .bridge()
        .deliveries
        .get(&request)
        .is_none_or(|delivery| {
            !matches!(delivery.phase, DeliveryPhase::AwaitingTerminal { pane: owner, .. } if owner == pane)
        })
    {
        return;
    }
    if let Some(delivery) = host.bridge().deliveries.remove(&request) {
        let DeliveryPhase::AwaitingTerminal { fd, .. } = delivery.phase else {
            unreachable!();
        };
        drop(delivery.sources);
        if let Some(data) = data.filter(|data| data.len() <= MAX_BYTES) {
            write(host, fd, data.into(), delivery.deadline);
        }
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
    host.bridge().set_export(target, None);
    host.set_selection(target, mimes, Source::Terminal { pane, offer });
    if intent == OfferIntent::Paste && target == SelectionTarget::Clipboard {
        host.paste(pane);
    }
}

/// Replace the seat clipboard with local text and paste it into a pane.
pub fn paste<H: Host>(host: &mut H, pane: PaneId, text: &str) {
    if text.len() > MAX_TEXT || !host.pane_has_window(pane) {
        return;
    }
    host.bridge().set_export(SelectionTarget::Clipboard, None);
    let data = Arc::new(ClipboardData::text(text));
    let mimes = data.items.iter().map(|item| item.mime.clone()).collect();
    host.set_selection(SelectionTarget::Clipboard, mimes, Source::Local(data));
    host.paste(pane);
}

#[cfg(test)]
mod tests {
    use calloop::EventLoop;

    use super::*;

    type RequestAction = Box<dyn FnOnce(&mut FakeHost) -> io::Result<()>>;

    struct FakeHost {
        handle: Handle<Self>,
        bridge: Bridge<Self>,
        contains: bool,
        on_request: Option<RequestAction>,
        requests: Vec<String>,
        emitted: Vec<(PaneId, ServerToPane)>,
    }

    impl FakeHost {
        fn new() -> Self {
            let event_loop: &'static EventLoop<'static, Self> =
                Box::leak(Box::new(EventLoop::try_new().expect("event loop")));
            let handle = Handle::new(event_loop.handle(), |host| host);
            Self {
                handle,
                bridge: Bridge::default(),
                contains: true,
                on_request: None,
                requests: Vec::new(),
                emitted: Vec::new(),
            }
        }

        fn attach_export(&mut self, pane: PaneId, mimes: &[&str]) {
            let target = SelectionTarget::Clipboard;
            let id = self.bridge.id();
            self.bridge.set_export(
                target,
                Some(Export {
                    id,
                    target,
                    pane,
                    mimes: mimes.iter().map(|mime| (*mime).to_owned()).collect(),
                    active: None,
                    data: ClipboardData::default(),
                    bytes: 0,
                    limit: MAX_BYTES,
                    deadline: Deadline::new(TRANSFER_TIMEOUT),
                }),
            );
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
            true
        }

        fn request_selection(
            &mut self,
            _target: SelectionTarget,
            mime: String,
            _fd: OwnedFd,
        ) -> io::Result<()> {
            self.requests.push(mime);
            self.on_request.take().map_or(Ok(()), |action| action(self))
        }

        fn set_selection(
            &mut self,
            _target: SelectionTarget,
            _mimes: Vec<String>,
            _source: Source,
        ) {
        }

        fn paste(&mut self, _pane: PaneId) {}
    }

    #[test]
    fn reentrant_requests_discard_in_flight_export() {
        let pane = PaneId::from(7);
        let target = SelectionTarget::Clipboard;
        let mut host = FakeHost::new();

        host.attach_export(pane, &["text/plain", "text/html"]);
        host.on_request = Some(Box::new(move |host| {
            host.bridge.detach(pane);
            Ok(())
        }));
        export_next(&mut host, target);
        assert_eq!(host.requests.len(), 1);
        assert!(host.bridge.export(target).is_none());
        assert!(host.emitted.is_empty());

        host.requests.clear();
        host.attach_export(pane, &["text/plain", "text/html"]);
        host.on_request = Some(Box::new(move |host| {
            host.bridge.set_export(target, None);
            Err(io::Error::other("request failed"))
        }));
        export_next(&mut host, target);
        assert_eq!(host.requests.len(), 1);
        assert!(host.bridge.export(target).is_none());

        host.requests.clear();
        host.attach_export(pane, &["text/plain", "text/html"]);
        host.on_request = Some(Box::new(|_| Err(io::Error::other("request failed"))));
        export_next(&mut host, target);
        assert_eq!(
            host.requests.iter().map(String::as_str).collect::<Vec<_>>(),
            ["text/plain", "text/html"]
        );
        assert!(host.bridge.export(target).is_some());
        assert!(host.emitted.is_empty());
    }
}
