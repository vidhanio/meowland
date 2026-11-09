//! Kitty OSC 5522 selection offers, permission-aware reads, and copy writes.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::{self, Write as _},
    mem,
    time::{Duration, Instant},
};

use base64::{Engine, display::Base64Display, engine::general_purpose::STANDARD};
use meowland_protocol::{
    ClipboardOfferId, ClipboardRequestId, OfferIntent,
    clipboard::{
        ClipboardData, Deadline, MAX_BYTES, MAX_TYPES, Selection, TRANSFER_TIMEOUT, mime_types,
    },
};

use crate::clipboard::{Action, Focus, Refresh};

const MAX_REQUESTS: usize = 16;
const MAX_OFFERS: usize = 16;
/// How long the terminal may take to list a selection's MIME types.
const LIST_TIMEOUT: Duration = Duration::from_secs(1);
/// Kitty's OSC 5522 payload limit per escape, applied before base64 encoding.
///
/// The protocol asks senders to split a representation into chunks of no more
/// than this and to finish one MIME type before starting the next; receivers
/// use the same bound to reject oversized packets. It also bounds the decoded
/// metadata fields, which arrive as single chunks.
const MAX_CHUNK_BYTES: usize = 4096;
/// Outgoing data chunk size, one byte below the protocol limit.
const WRITE_CHUNK_BYTES: usize = MAX_CHUNK_BYTES - 1;
/// Room for every accepted MIME type at its maximum length plus a separator.
const MAX_TYPE_LIST: usize = MAX_TYPES * 257;
/// Human-readable requester name sent with the session password.
const SESSION_NAME: &str = "meowland";
/// Requester name the protocol prescribes when using a paste event's one-time
/// password.
const PASTE_EVENT_NAME: &str = "Paste event";
/// Payload asking the terminal to list a selection's MIME types.
const LIST_PAYLOAD: &str = ".";

/// Wire identity of one request, echoed by the terminal in every reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OperationId(u64);

impl OperationId {
    const PREFIX: &str = "meowland-";

    fn parse(id: &str) -> Option<Self> {
        id.strip_prefix(Self::PREFIX)?.parse().ok().map(Self)
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", Self::PREFIX, self.0)
    }
}

/// The selection an offer came from and the terminal's one-time paste
/// password, which the protocol allows to be used once.
struct Credentials {
    selection: Selection,
    password: Option<String>,
}

/// A read reply that has delivered its `OK` header and all `DATA` packets.
struct Received {
    bytes: Vec<u8>,
    credentials: Credentials,
}

/// Progress of an OSC 5522 read reply.
enum Reading {
    AwaitingOk,
    Receiving {
        bytes: Vec<u8>,
        credentials: Credentials,
    },
}

enum Progress {
    Pending(Reading),
    Complete(Received),
    Failed,
}

impl Reading {
    fn accept(self, packet: &Packet<'_>, expected: &str, limit: usize) -> Progress {
        match (self, packet.status) {
            (Self::AwaitingOk, Status::Ok) => Progress::Pending(Self::Receiving {
                bytes: Vec::new(),
                credentials: Credentials {
                    selection: packet.selection,
                    password: packet.password.clone(),
                },
            }),
            (Self::Receiving { credentials, .. }, _)
                if packet
                    .password
                    .as_ref()
                    .is_some_and(|password| credentials.password.as_ref() != Some(password)) =>
            {
                Progress::Failed
            }
            (
                Self::Receiving {
                    mut bytes,
                    credentials,
                },
                Status::Data,
            ) if packet.mime.as_deref() == Some(expected) => {
                let Ok(data) = STANDARD.decode(packet.payload) else {
                    return Progress::Failed;
                };
                if data.len() > MAX_CHUNK_BYTES || bytes.len() + data.len() > limit {
                    return Progress::Failed;
                }
                bytes.extend_from_slice(&data);
                Progress::Pending(Self::Receiving { bytes, credentials })
            }
            (Self::Receiving { bytes, credentials }, Status::Done) => {
                Progress::Complete(Received { bytes, credentials })
            }
            _ => Progress::Failed,
        }
    }
}

enum Request {
    List {
        selection: Selection,
        refresh: Refresh,
        epoch: u64,
    },
    Read {
        selection: Selection,
        request: ClipboardRequestId,
        offer: ClipboardOfferId,
        mime: String,
        deadline: Deadline,
    },
    Write {
        selection: Selection,
        data: ClipboardData,
    },
}

impl Request {
    const fn selection(&self) -> Selection {
        match self {
            Self::List { selection, .. }
            | Self::Read { selection, .. }
            | Self::Write { selection, .. } => *selection,
        }
    }
}

struct Queued {
    request: Request,
    deadline: Instant,
}

/// Who receives the result of a representation read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    To(ClipboardRequestId),
    /// The server withdrew the request; the terminal's reply is consumed
    /// silently to keep later replies correlated.
    Cancelled,
}

/// What an in-flight request is waiting for.
enum Purpose {
    List {
        refresh: Refresh,
        epoch: u64,
        reading: Reading,
    },
    Read {
        reply: Reply,
        mime: String,
        reading: Reading,
    },
    Write,
}

/// A request whose header was sent and whose reply is awaited.
struct Operation {
    id: OperationId,
    selection: Selection,
    deadline: Instant,
    purpose: Purpose,
}

/// Whether a streaming write's `DONE` reply is still awaited.
enum WriteReply {
    Awaiting {
        id: OperationId,
        deadline: Instant,
    },
    /// The write timed out or the terminal reported an error; the payload
    /// still streams to its terminator so the terminal sees a complete
    /// transfer before the next request.
    Abandoned,
}

/// Remaining `wdata` packets of a write.
struct Stream {
    data: ClipboardData,
    item: usize,
    offset: usize,
}

impl Stream {
    /// Append the next `wdata` packet, returning true once the terminator was
    /// written.
    fn write_chunk(&mut self, output: &mut Vec<u8>) -> bool {
        let Some(item) = self.data.items.get(self.item) else {
            packet(output, "type=wdata", b"");
            return true;
        };
        let end = (self.offset + WRITE_CHUNK_BYTES).min(item.data.len());
        packet(
            output,
            format_args!(
                "type=wdata:mime={}",
                Base64Display::new(item.mime.as_bytes(), &STANDARD)
            ),
            &item.data[self.offset..end],
        );
        if end == item.data.len() {
            self.item += 1;
            self.offset = 0;
        } else {
            self.offset = end;
        }
        false
    }
}

/// The single request the terminal is serving.
#[derive(Default)]
enum Channel {
    #[default]
    Idle,
    Awaiting(Operation),
    Streaming {
        selection: Selection,
        stream: Stream,
        reply: WriteReply,
    },
}

impl Channel {
    const fn selection(&self) -> Option<Selection> {
        match self {
            Self::Idle => None,
            Self::Awaiting(operation) => Some(operation.selection),
            Self::Streaming { selection, .. } => Some(*selection),
        }
    }

    const fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Awaiting(operation) => Some(operation.deadline),
            Self::Streaming {
                reply: WriteReply::Awaiting { deadline, .. },
                ..
            } => Some(*deadline),
            Self::Idle
            | Self::Streaming {
                reply: WriteReply::Abandoned,
                ..
            } => None,
        }
    }
}

/// Bounded OSC 5522 selections with serialized requests and session
/// credentials.
pub(crate) struct Clipboard {
    password: String,
    focus: Focus,
    /// Explicit pastes supersede MIME refreshes issued before them.
    epochs: [u64; 2],
    next_operation: u64,
    next_offer: u64,
    requests: VecDeque<Queued>,
    channel: Channel,
    notification: Option<Reading>,
    offers: HashMap<ClipboardOfferId, Credentials>,
    actions: VecDeque<Action>,
}

impl fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clipboard")
            .field("focus", &self.focus)
            .field("queued_requests", &self.requests.len())
            .finish_non_exhaustive()
    }
}

impl Clipboard {
    /// Create clipboard state with secure credentials for terminal permission
    /// grants.
    ///
    /// # Errors
    /// Returns an error if secure request credentials cannot be generated.
    pub(crate) fn new() -> io::Result<Self> {
        let mut password = [0; 32];
        getrandom::fill(&mut password).map_err(io::Error::other)?;
        Ok(Self {
            password: STANDARD.encode(password),
            focus: Focus::Gained,
            epochs: [0; 2],
            next_operation: 0,
            next_offer: 0,
            requests: VecDeque::new(),
            channel: Channel::Idle,
            notification: None,
            offers: HashMap::new(),
            actions: VecDeque::new(),
        })
    }

    /// Resume background refreshes on focus entry and stop them on focus loss.
    pub(crate) fn focus(&mut self, focus: Focus, now: Instant) {
        self.focus = focus;
        match focus {
            Focus::Gained => {
                let _ = self.refresh(Selection::Clipboard, Refresh::Background, now);
                let _ = self.refresh(Selection::Primary, Refresh::Background, now);
            }
            Focus::Lost => {
                self.notification = None;
                self.cancel_lists(Selection::Clipboard);
                self.cancel_lists(Selection::Primary);
            }
        }
    }

    /// Supersede queued metadata refreshes for one selection.
    pub(crate) fn cancel_refresh_selection(&mut self, selection: Selection) {
        self.cancel_lists(selection);
    }

    /// Request MIME metadata, returning the deadline for deferred paste input.
    pub(crate) fn refresh(
        &mut self,
        selection: Selection,
        refresh: Refresh,
        now: Instant,
    ) -> Option<Instant> {
        if refresh == Refresh::Paste && self.has_selection(selection) {
            return None;
        }
        self.enqueue(
            Request::List {
                selection,
                refresh,
                epoch: self.epoch(selection),
            },
            now,
        )
    }

    /// Queue a representation read from a previously advertised selection
    /// offer.
    pub(crate) fn read(
        &mut self,
        request: ClipboardRequestId,
        offer: ClipboardOfferId,
        mime: String,
        deadline: Deadline,
        now: Instant,
    ) {
        let selection = self
            .offers
            .get(&offer)
            .map_or(Selection::Clipboard, |credentials| credentials.selection);
        let _ = self.enqueue(
            Request::Read {
                selection,
                request,
                offer,
                mime,
                deadline,
            },
            now,
        );
    }

    /// Suppress a pending representation read and any queued reply for it.
    pub(crate) fn cancel(&mut self, request: ClipboardRequestId) {
        self.requests.retain(|queued| {
            !matches!(queued.request, Request::Read { request: number, .. } if number == request)
        });
        if let Channel::Awaiting(Operation {
            purpose: Purpose::Read { reply, .. },
            ..
        }) = &mut self.channel
            && *reply == Reply::To(request)
        {
            *reply = Reply::Cancelled;
        }
        self.actions.retain(
            |action| !matches!(action, Action::Reply { request: number, .. } if *number == request),
        );
    }

    const fn epoch(&self, selection: Selection) -> u64 {
        self.epochs[selection_index(selection)]
    }

    fn has_selection(&self, selection: Selection) -> bool {
        self.channel.selection() == Some(selection)
            || self
                .requests
                .iter()
                .any(|queued| queued.request.selection() == selection)
    }

    fn cancel_lists(&mut self, selection: Selection) {
        self.epochs[selection_index(selection)] += 1;
        let mut unblock = false;
        self.requests.retain(|queued| match &queued.request {
            Request::List {
                selection: target,
                refresh,
                ..
            } if *target == selection => {
                unblock |= *refresh == Refresh::Paste;
                false
            }
            _ => true,
        });
        if unblock {
            self.actions.push_back(Action::Unblock);
        }
    }

    /// Queue bounded MIME-aware clipboard contents for the selected
    /// destination.
    pub(crate) fn copy(&mut self, selection: Selection, data: ClipboardData, now: Instant) {
        if !data.valid() {
            return;
        }
        let _ = self.enqueue(Request::Write { selection, data }, now);
    }

    fn enqueue(&mut self, request: Request, now: Instant) -> Option<Instant> {
        let timeout = match &request {
            Request::List { .. } => LIST_TIMEOUT,
            Request::Read { deadline, .. } => deadline.remaining(),
            Request::Write { .. } => TRANSFER_TIMEOUT,
        };
        if timeout.is_zero() {
            self.fail_request(&request);
            return None;
        }
        if let Request::Write { selection, .. } = request {
            self.requests.retain(|queued| {
                !matches!(queued.request, Request::Write { selection: target, .. } if target == selection)
            });
            self.cancel_lists(selection);
        }
        if self.requests.len() >= MAX_REQUESTS {
            tracing::warn!("Clipboard request queue is full");
            self.fail_request(&request);
            return None;
        }
        let deadline = now + timeout;
        self.requests.push_back(Queued { request, deadline });
        Some(deadline)
    }

    /// Pop the next selection or input action in protocol order.
    pub(crate) fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    /// Earliest request deadline that should wake the terminal session.
    #[must_use]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.channel
            .deadline()
            .into_iter()
            .chain(self.requests.iter().map(|queued| queued.deadline))
            .min()
    }

    fn expire(&mut self, now: Instant) {
        self.channel = match mem::take(&mut self.channel) {
            Channel::Awaiting(operation) if now >= operation.deadline => {
                tracing::warn!(id = %operation.id, "Terminal clipboard request timed out");
                self.fail_purpose(&operation.purpose);
                Channel::Idle
            }
            Channel::Streaming {
                selection,
                stream,
                reply: WriteReply::Awaiting { id, deadline },
            } if now >= deadline => {
                tracing::warn!(%id, "Terminal clipboard write timed out");
                Channel::Streaming {
                    selection,
                    stream,
                    reply: WriteReply::Abandoned,
                }
            }
            channel => channel,
        };
        let mut requests = mem::take(&mut self.requests);
        requests.retain(|queued| {
            if now < queued.deadline {
                true
            } else {
                self.fail_request(&queued.request);
                false
            }
        });
        self.requests = requests;
    }

    /// Expire requests and append the next transfer when output is idle.
    pub(crate) fn advance(&mut self, now: Instant, output: &mut Vec<u8>) {
        self.expire(now);
        if !output.is_empty() {
            return;
        }
        self.channel = match mem::take(&mut self.channel) {
            Channel::Streaming {
                selection,
                mut stream,
                reply,
            } => {
                if !stream.write_chunk(output) {
                    Channel::Streaming {
                        selection,
                        stream,
                        reply,
                    }
                } else if let WriteReply::Awaiting { id, deadline } = reply {
                    Channel::Awaiting(Operation {
                        id,
                        selection,
                        deadline,
                        purpose: Purpose::Write,
                    })
                } else {
                    Channel::Idle
                }
            }
            Channel::Idle => self.start_next(output),
            channel @ Channel::Awaiting(_) => channel,
        };
    }

    fn start_next(&mut self, output: &mut Vec<u8>) -> Channel {
        while let Some(Queued { request, deadline }) = self.requests.pop_front() {
            self.next_operation += 1;
            let id = OperationId(self.next_operation);
            let selection = request.selection();
            let session = Requester {
                id,
                name: SESSION_NAME,
                password: &self.password,
                selection,
            };
            let purpose = match request {
                Request::List { refresh, epoch, .. } => {
                    packet(
                        output,
                        format_args!("type=read:{session}"),
                        LIST_PAYLOAD.as_bytes(),
                    );
                    Purpose::List {
                        refresh,
                        epoch,
                        reading: Reading::AwaitingOk,
                    }
                }
                Request::Read {
                    request,
                    offer,
                    mime,
                    ..
                } => {
                    let Some(credentials) = self.offers.get_mut(&offer) else {
                        self.actions.push_back(Action::Reply {
                            request,
                            data: None,
                        });
                        continue;
                    };
                    match credentials.password.take() {
                        Some(password) => {
                            let paste = Requester {
                                name: PASTE_EVENT_NAME,
                                password: &password,
                                ..session
                            };
                            packet(output, format_args!("type=read:{paste}"), mime.as_bytes());
                        }
                        None => {
                            packet(output, format_args!("type=read:{session}"), mime.as_bytes());
                        }
                    }
                    Purpose::Read {
                        reply: Reply::To(request),
                        mime,
                        reading: Reading::AwaitingOk,
                    }
                }
                Request::Write { data, .. } => {
                    packet(output, format_args!("type=write:{session}"), b"");
                    return Channel::Streaming {
                        selection,
                        stream: Stream {
                            data,
                            item: 0,
                            offset: 0,
                        },
                        reply: WriteReply::Awaiting { id, deadline },
                    };
                }
            };
            return Channel::Awaiting(Operation {
                id,
                selection,
                deadline,
                purpose,
            });
        }
        Channel::Idle
    }

    /// Accept an OSC 5522 reply or paste notification from the terminal.
    pub(crate) fn packet(&mut self, bytes: &[u8], now: Instant) {
        self.expire(now);
        let Some(packet) = Packet::parse(bytes) else {
            return;
        };
        let Some(id) = packet.id else {
            if packet.kind == Kind::Read {
                self.notification(&packet);
            }
            return;
        };
        let id = OperationId::parse(id);
        self.channel = match mem::take(&mut self.channel) {
            Channel::Awaiting(operation) if id == Some(operation.id) => {
                self.reply(operation, &packet)
            }
            Channel::Streaming {
                selection,
                stream,
                reply: WriteReply::Awaiting { id: awaited, .. },
            } if id == Some(awaited) => {
                tracing::warn!(
                    status = ?packet.status,
                    id = %awaited,
                    "Terminal clipboard write ended before its data was sent"
                );
                Channel::Streaming {
                    selection,
                    stream,
                    reply: WriteReply::Abandoned,
                }
            }
            channel => channel,
        };
    }

    fn notification(&mut self, packet: &Packet<'_>) {
        if packet.status == Status::Ok && self.focus == Focus::Gained {
            self.notification = Some(Reading::AwaitingOk);
        }
        let Some(reading) = self.notification.take() else {
            return;
        };
        match reading.accept(packet, LIST_PAYLOAD, MAX_TYPE_LIST) {
            Progress::Pending(reading) => self.notification = Some(reading),
            Progress::Complete(received) => {
                let selection = received.credentials.selection;
                self.cancel_lists(selection);
                self.offer(received, selection, OfferIntent::Paste);
                self.actions.push_back(Action::Unblock);
            }
            Progress::Failed => {}
        }
    }

    fn reply(&mut self, operation: Operation, packet: &Packet<'_>) -> Channel {
        let Operation {
            id,
            selection,
            deadline,
            purpose,
        } = operation;
        let progress = match (purpose, packet.kind) {
            (Purpose::Write, Kind::Write) if packet.status == Status::Done => return Channel::Idle,
            (
                Purpose::List {
                    refresh,
                    epoch,
                    reading,
                },
                Kind::Read,
            ) => match reading.accept(packet, LIST_PAYLOAD, MAX_TYPE_LIST) {
                Progress::Pending(reading) => Ok(Purpose::List {
                    refresh,
                    epoch,
                    reading,
                }),
                Progress::Complete(received) => {
                    if epoch == self.epoch(selection) {
                        self.offer(received, selection, OfferIntent::Refresh);
                    }
                    if refresh == Refresh::Paste {
                        self.actions.push_back(Action::Unblock);
                    }
                    return Channel::Idle;
                }
                Progress::Failed => Err(Purpose::List {
                    refresh,
                    epoch,
                    reading: Reading::AwaitingOk,
                }),
            },
            (
                Purpose::Read {
                    reply,
                    mime,
                    reading,
                },
                Kind::Read,
            ) => match reading.accept(packet, &mime, MAX_BYTES) {
                Progress::Pending(reading) => Ok(Purpose::Read {
                    reply,
                    mime,
                    reading,
                }),
                Progress::Complete(received) => {
                    if let Reply::To(request) = reply {
                        self.actions.push_back(Action::Reply {
                            request,
                            data: Some(received.bytes),
                        });
                    }
                    return Channel::Idle;
                }
                Progress::Failed => Err(Purpose::Read {
                    reply,
                    mime,
                    reading: Reading::AwaitingOk,
                }),
            },
            (purpose, _) => Err(purpose),
        };
        match progress {
            Ok(purpose) => Channel::Awaiting(Operation {
                id,
                selection,
                deadline,
                purpose,
            }),
            Err(purpose) => {
                if !matches!(
                    purpose,
                    Purpose::Read {
                        reply: Reply::Cancelled,
                        ..
                    }
                ) {
                    tracing::warn!(
                        status = ?packet.status,
                        %id,
                        "Terminal clipboard request failed"
                    );
                }
                self.fail_purpose(&purpose);
                Channel::Idle
            }
        }
    }

    fn offer(&mut self, received: Received, selection: Selection, intent: OfferIntent) {
        let Ok(types) = String::from_utf8(received.bytes) else {
            return;
        };
        let mimes = mime_types(types.split_ascii_whitespace().map(str::to_owned));
        self.next_offer += 1;
        let offer = ClipboardOfferId::from(self.next_offer);
        if self.offers.len() >= MAX_OFFERS
            && let Some(oldest) = self.offers.keys().min().copied()
        {
            self.offers.remove(&oldest);
        }
        self.offers.insert(
            offer,
            Credentials {
                selection,
                password: received.credentials.password,
            },
        );
        self.actions.push_back(Action::Offer {
            selection,
            offer,
            mimes,
            intent,
        });
    }

    fn fail_request(&mut self, request: &Request) {
        match *request {
            Request::List {
                refresh: Refresh::Paste,
                ..
            } => self.actions.push_back(Action::Unblock),
            Request::Read { request, .. } => self.actions.push_back(Action::Reply {
                request,
                data: None,
            }),
            Request::List { .. } | Request::Write { .. } => {}
        }
    }

    fn fail_purpose(&mut self, purpose: &Purpose) {
        match *purpose {
            Purpose::List {
                refresh: Refresh::Paste,
                ..
            } => self.actions.push_back(Action::Unblock),
            Purpose::Read {
                reply: Reply::To(request),
                ..
            } => self.actions.push_back(Action::Reply {
                request,
                data: None,
            }),
            Purpose::List { .. } | Purpose::Read { .. } | Purpose::Write => {}
        }
    }
}

const fn selection_index(selection: Selection) -> usize {
    match selection {
        Selection::Clipboard => 0,
        Selection::Primary => 1,
    }
}

/// Request metadata identifying the requester to the terminal's permission
/// prompt.
#[derive(Clone, Copy)]
struct Requester<'a> {
    id: OperationId,
    name: &'a str,
    password: &'a str,
    selection: Selection,
}

impl fmt::Display for Requester<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "id={}:name={}:pw={}",
            self.id,
            Base64Display::new(self.name.as_bytes(), &STANDARD),
            Base64Display::new(self.password.as_bytes(), &STANDARD)
        )?;
        if self.selection == Selection::Primary {
            f.write_str(":loc=primary")?;
        }
        Ok(())
    }
}

fn packet(output: &mut Vec<u8>, metadata: impl fmt::Display, payload: &[u8]) {
    write!(
        output,
        "\x1b]5522;{metadata};{}\x1b\\",
        Base64Display::new(payload, &STANDARD)
    )
    .expect("writing to a Vec cannot fail");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status<'a> {
    Ok,
    Data,
    Done,
    Error(&'a str),
}

impl<'a> Status<'a> {
    fn parse(status: &'a str) -> Self {
        match status {
            "OK" => Self::Ok,
            "DATA" => Self::Data,
            "DONE" => Self::Done,
            error => Self::Error(error),
        }
    }
}

struct Packet<'a> {
    kind: Kind,
    status: Status<'a>,
    id: Option<&'a str>,
    mime: Option<String>,
    password: Option<String>,
    selection: Selection,
    payload: &'a str,
}

impl<'a> Packet<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (metadata, payload) = text.split_once(';').unwrap_or((text, ""));
        let mut fields = HashMap::new();
        for field in metadata.split(':') {
            let (key, value) = field.split_once('=')?;
            if fields.insert(key, value).is_some() {
                return None;
            }
        }
        let decode = |key| {
            let decoded = STANDARD.decode(fields.get(key)?).ok()?;
            if decoded.len() > MAX_CHUNK_BYTES {
                return None;
            }
            String::from_utf8(decoded).ok()
        };
        let optional = |key| {
            if fields.contains_key(key) {
                decode(key).map(Some)
            } else {
                Some(None)
            }
        };
        let kind = match *fields.get("type")? {
            "read" => Kind::Read,
            "write" => Kind::Write,
            _ => return None,
        };
        Some(Self {
            kind,
            status: Status::parse(fields.get("status")?),
            id: fields.get("id").copied(),
            mime: optional("mime")?,
            password: optional("pw")?,
            selection: if fields.get("loc") == Some(&"primary") {
                Selection::Primary
            } else {
                Selection::Clipboard
            },
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use meowland_protocol::clipboard::Item;

    use super::*;

    /// Split OSC 5522 escapes into metadata and decoded payload.
    fn decode_packets(output: &[u8]) -> Vec<(String, Vec<u8>)> {
        let text = std::str::from_utf8(output).expect("packets are ASCII");
        text.split_terminator("\x1b\\")
            .map(|packet| {
                let body = packet
                    .strip_prefix("\x1b]5522;")
                    .expect("every packet starts with the OSC 5522 introducer");
                let (metadata, payload) = body.split_once(';').expect("payload separator");
                let payload = STANDARD.decode(payload).expect("base64 payload");
                (metadata.to_owned(), payload)
            })
            .collect()
    }

    #[test]
    fn write_packets_reassemble_items_across_chunk_boundaries() {
        let items = vec![
            Item {
                mime: "text/plain".to_owned(),
                data: (0..=u8::MAX)
                    .cycle()
                    .take(WRITE_CHUNK_BYTES * 2 + 1)
                    .collect(),
            },
            Item {
                mime: "image/png".to_owned(),
                data: Vec::new(),
            },
            Item {
                mime: "text/html".to_owned(),
                data: vec![7; WRITE_CHUNK_BYTES],
            },
        ];
        let now = Instant::now();
        let mut clipboard = Clipboard::new().expect("clipboard credentials");
        clipboard.copy(
            Selection::Clipboard,
            ClipboardData {
                items: items.clone(),
            },
            now,
        );
        let mut output = Vec::new();
        let mut wire = Vec::new();
        loop {
            clipboard.advance(now, &mut output);
            assert!(
                !output.is_empty(),
                "each drain must be followed by write progress"
            );
            assert!(output.len() < 8192, "write output must remain bounded");
            let queued_len = output.len();
            clipboard.advance(now, &mut output);
            assert_eq!(output.len(), queued_len, "undrained output must not grow");
            wire.extend_from_slice(&output);
            output.clear();
            if matches!(clipboard.channel, Channel::Awaiting(_)) {
                break;
            }
        }

        let mut expected_wire = format!(
            "\x1b]5522;type=write:id=meowland-1:name={}:pw={};\x1b\\",
            STANDARD.encode("meowland"),
            STANDARD.encode(&clipboard.password),
        )
        .into_bytes();
        for item in &items {
            let mime = STANDARD.encode(&item.mime);
            if item.data.is_empty() {
                expected_wire.extend_from_slice(
                    format!("\x1b]5522;type=wdata:mime={mime};\x1b\\").as_bytes(),
                );
            }
            for chunk in item.data.chunks(WRITE_CHUNK_BYTES) {
                expected_wire.extend_from_slice(
                    format!(
                        "\x1b]5522;type=wdata:mime={mime};{}\x1b\\",
                        STANDARD.encode(chunk)
                    )
                    .as_bytes(),
                );
            }
        }
        expected_wire.extend_from_slice(b"\x1b]5522;type=wdata;\x1b\\");
        assert_eq!(
            wire, expected_wire,
            "streaming must preserve the wire bytes"
        );

        let packets = decode_packets(&wire);
        let (header, rest) = packets.split_first().expect("write header");
        assert!(header.0.starts_with("type=write:id="), "{header:?}");
        assert_eq!(header.1, b"");
        let (terminator, chunks) = rest.split_last().expect("write terminator");
        assert_eq!(*terminator, ("type=wdata".to_owned(), Vec::new()));
        assert_eq!(chunks.len(), 3 + 1 + 1);
        let mut decoded: Vec<(String, Vec<u8>)> = Vec::new();
        for (metadata, payload) in chunks {
            assert!(payload.len() <= WRITE_CHUNK_BYTES);
            let mime = metadata
                .strip_prefix("type=wdata:mime=")
                .expect("data chunks carry their MIME type");
            let mime =
                String::from_utf8(STANDARD.decode(mime).expect("base64 MIME")).expect("UTF-8 MIME");
            match decoded.last_mut() {
                Some((current, data)) if *current == mime => data.extend_from_slice(payload),
                _ => decoded.push((mime, payload.clone())),
            }
        }
        let expected: Vec<_> = items
            .into_iter()
            .map(|item| (item.mime, item.data))
            .collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn timed_out_write_finishes_before_next_request_and_ignores_late_reply() {
        let now = Instant::now();
        let mut clipboard = Clipboard::new().expect("clipboard credentials");
        clipboard.copy(
            Selection::Clipboard,
            ClipboardData {
                items: vec![Item {
                    mime: "text/plain".to_owned(),
                    data: vec![1; WRITE_CHUNK_BYTES + 1],
                }],
            },
            now,
        );
        let mut output = Vec::new();
        clipboard.advance(now, &mut output);
        output.clear();
        assert_eq!(clipboard.deadline(), Some(now + TRANSFER_TIMEOUT));
        clipboard.copy(
            Selection::Primary,
            ClipboardData::default(),
            now + Duration::from_secs(1),
        );

        let expired = now + TRANSFER_TIMEOUT;
        let mut wire = Vec::new();
        while matches!(clipboard.channel, Channel::Streaming { .. }) {
            clipboard.advance(expired, &mut output);
            assert!(!output.is_empty(), "timed-out write must still finish");
            wire.extend_from_slice(&output);
            output.clear();
        }
        let packets = decode_packets(&wire);
        assert_eq!(packets.len(), 3, "two data chunks and one terminator");
        assert_eq!(packets.last().expect("terminator").0, "type=wdata");

        clipboard.advance(expired, &mut output);
        let header = decode_packets(&output);
        assert!(header[0].0.starts_with("type=write:id=meowland-2:"));
        clipboard.packet(b"type=write:status=DONE:id=meowland-1;", expired);
        assert!(
            matches!(
                clipboard.channel,
                Channel::Streaming {
                    reply: WriteReply::Awaiting {
                        id: OperationId(2),
                        ..
                    },
                    ..
                }
            ),
            "a late reply must not finish the next request"
        );
    }

    #[test]
    fn packet_parses_metadata_and_payload() {
        let packet = Packet::parse(
            b"type=read:status=OK:id=abc:mime=dGV4dC9wbGFpbg==:pw=c2VjcmV0:loc=primary;cGF5bG9hZA==",
        )
        .expect("valid packet");
        assert_eq!(packet.kind, Kind::Read);
        assert_eq!(packet.status, Status::Ok);
        assert_eq!(packet.id, Some("abc"));
        assert_eq!(packet.mime.as_deref(), Some("text/plain"));
        assert_eq!(packet.password.as_deref(), Some("secret"));
        assert_eq!(packet.selection, Selection::Primary);
        assert_eq!(packet.payload, "cGF5bG9hZA==");
    }

    #[test]
    fn packet_parses_optional_fields_and_defaults() {
        let packet = Packet::parse(b"type=write:status=DONE;").expect("valid packet");
        assert_eq!(packet.kind, Kind::Write);
        assert_eq!(packet.status, Status::Done);
        assert_eq!(packet.id, None);
        assert_eq!(packet.mime, None);
        assert_eq!(packet.password, None);
        assert_eq!(packet.selection, Selection::Clipboard);
        assert_eq!(packet.payload, "");

        let packet =
            Packet::parse(b"type=read:status=OK:loc=secondary;payload").expect("valid packet");
        assert_eq!(packet.selection, Selection::Clipboard);
        assert_eq!(packet.payload, "payload");
    }

    #[test]
    fn packet_preserves_semicolons_in_payload() {
        let packet = Packet::parse(b"type=read:status=OK;one;two").expect("valid packet");
        assert_eq!(packet.payload, "one;two");
    }

    #[test]
    fn packet_rejects_duplicate_keys() {
        for bytes in [
            &b"type=read:type=write:status=OK;"[..],
            &b"type=read:status=OK:status=DONE;"[..],
            &b"type=read:status=OK:id=a:id=b;"[..],
            &b"type=read:status=OK:mime=dGV4dA==:mime=dGV4dA==;"[..],
        ] {
            assert!(Packet::parse(bytes).is_none(), "packet {bytes:?}");
        }
    }

    #[test]
    fn packet_rejects_malformed_metadata() {
        for bytes in [
            &b""[..],
            &b";"[..],
            &b"type=read:status;"[..],
            &b"status=OK;"[..],
            &b"type=read;"[..],
            &b"type=read:status=OK:;"[..],
        ] {
            assert!(Packet::parse(bytes).is_none(), "packet {bytes:?}");
        }
    }

    #[test]
    fn packet_rejects_invalid_base64_and_utf8() {
        for bytes in [
            &b"type=read:status=OK:mime=%%%;"[..],
            &b"type=read:status=OK:pw=%%%;"[..],
            &b"type=read:status=OK:mime=/w==;"[..],
            &b"type=read:status=OK:pw=/w==;"[..],
            &b"type=read:status=OK;\xff"[..],
        ] {
            assert!(Packet::parse(bytes).is_none(), "packet {bytes:?}");
        }
    }

    #[test]
    fn packet_rejects_oversized_decoded_fields() {
        let oversized = STANDARD.encode(vec![b'a'; MAX_CHUNK_BYTES + 1]);
        for field in ["mime", "pw"] {
            let packet = format!("type=read:status=OK:{field}={oversized};");
            assert!(Packet::parse(packet.as_bytes()).is_none(), "field {field}");
        }
    }
}
