//! Kitty OSC 5522 selection offers, permission-aware reads, and copy writes.

use std::{
    collections::{HashMap, VecDeque},
    io,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use meowland_protocol::clipboard::{
    ClipboardData, Deadline, MAX_BYTES, MAX_TYPES, Selection, TRANSFER_TIMEOUT, mime_types,
};

use super::Action;

const MAX_REQUESTS: usize = 16;
const MAX_OFFERS: usize = 16;
/// Room for every accepted MIME type at its maximum length plus a separator.
const MAX_TYPE_LIST: usize = MAX_TYPES * 257;

#[derive(Default)]
struct Credentials {
    primary: bool,
    password: Option<String>,
}

struct Reading {
    started: bool,
    bytes: Vec<u8>,
    credentials: Credentials,
}

impl Reading {
    fn new() -> Self {
        Self {
            started: false,
            bytes: Vec::new(),
            credentials: Credentials::default(),
        }
    }

    fn accept(&mut self, packet: &Packet<'_>, expected: &str, limit: usize) -> Option<bool> {
        if self.started
            && packet
                .password
                .as_ref()
                .is_some_and(|password| self.credentials.password.as_ref() != Some(password))
        {
            return None;
        }
        match packet.status {
            "OK" if !self.started => {
                self.started = true;
                self.credentials.primary = packet.primary;
                self.credentials.password.clone_from(&packet.password);
            }
            "DATA" if self.started && packet.mime.as_deref() == Some(expected) => {
                let data = STANDARD.decode(packet.payload).ok()?;
                if data.len() > 4096 || self.bytes.len() + data.len() > limit {
                    return None;
                }
                self.bytes.extend_from_slice(&data);
            }
            "DONE" if self.started => return Some(true),
            _ => return None,
        }
        Some(false)
    }
}

enum Request {
    List {
        selection: Selection,
        shortcut: bool,
        epoch: u64,
    },
    Read {
        selection: Selection,
        request: u64,
        offer: u64,
        mime: String,
        deadline: Deadline,
    },
    Write {
        selection: Selection,
        data: ClipboardData,
    },
}

struct Queued {
    request: Request,
    deadline: Instant,
}

struct Operation {
    id: String,
    request: Request,
    reading: Reading,
    deadline: Instant,
    cancelled: bool,
}

pub struct Clipboard {
    password: String,
    focused: bool,
    /// Explicit pastes supersede MIME refreshes issued before them.
    epochs: [u64; 2],
    next: u64,
    requests: VecDeque<Queued>,
    active: Option<Operation>,
    notification: Option<Reading>,
    offers: HashMap<u64, Credentials>,
    actions: VecDeque<Action>,
}

impl Clipboard {
    pub fn new() -> io::Result<Self> {
        let mut password = [0; 32];
        getrandom::fill(&mut password).map_err(io::Error::other)?;
        Ok(Self {
            password: STANDARD.encode(password),
            focused: true,
            epochs: [0; 2],
            next: 0,
            requests: VecDeque::new(),
            active: None,
            notification: None,
            offers: HashMap::new(),
            actions: VecDeque::new(),
        })
    }

    pub fn focus(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            let _ = self.refresh(Selection::Clipboard, false);
            let _ = self.refresh(Selection::Primary, false);
        } else {
            self.cancel_refresh();
        }
    }

    pub fn cancel_refresh(&mut self) {
        self.notification = None;
        self.cancel_lists(Selection::Clipboard);
        self.cancel_lists(Selection::Primary);
    }

    pub fn cancel_refresh_selection(&mut self, selection: Selection) {
        self.cancel_lists(selection);
    }

    pub fn refresh(&mut self, selection: Selection, shortcut: bool) -> Option<Instant> {
        if shortcut && self.has_selection(selection) {
            return None;
        }
        self.enqueue(Request::List {
            selection,
            shortcut,
            epoch: self.epoch(selection),
        })
    }

    pub fn read(&mut self, request: u64, offer: u64, mime: String, deadline: Deadline) {
        let selection = self
            .offers
            .get(&offer)
            .map_or(Selection::Clipboard, |credentials| {
                if credentials.primary {
                    Selection::Primary
                } else {
                    Selection::Clipboard
                }
            });
        let _ = self.enqueue(Request::Read {
            selection,
            request,
            offer,
            mime,
            deadline,
        });
    }

    pub fn cancel(&mut self, request: u64) {
        let matches = |candidate: &Request| matches!(candidate, Request::Read { request: number, .. } if *number == request);
        self.requests.retain(|queued| !matches(&queued.request));
        if let Some(operation) = &mut self.active
            && matches(&operation.request)
        {
            operation.cancelled = true;
        }
        self.actions.retain(
            |action| !matches!(action, Action::Reply { request: number, .. } if *number == request),
        );
    }

    const fn epoch(&self, selection: Selection) -> u64 {
        self.epochs[selection_index(selection)]
    }

    fn has_selection(&self, selection: Selection) -> bool {
        self.active
            .as_ref()
            .is_some_and(|operation| request_selection(&operation.request) == selection)
            || self
                .requests
                .iter()
                .any(|queued| request_selection(&queued.request) == selection)
    }

    fn cancel_lists(&mut self, selection: Selection) {
        self.epochs[selection_index(selection)] += 1;
        let mut unblock = false;
        self.requests.retain(|queued| match &queued.request {
            Request::List {
                selection: target,
                shortcut,
                ..
            } if *target == selection => {
                unblock |= *shortcut;
                false
            }
            _ => true,
        });
        if unblock {
            self.actions.push_back(Action::Unblock);
        }
    }

    pub fn copy(&mut self, selection: Selection, data: ClipboardData) {
        if !data.valid() {
            return;
        }
        let _ = self.enqueue(Request::Write { selection, data });
    }

    fn enqueue(&mut self, request: Request) -> Option<Instant> {
        let timeout = match &request {
            Request::List { .. } => Duration::from_secs(1),
            Request::Read { deadline, .. } => deadline.remaining(),
            Request::Write { .. } => TRANSFER_TIMEOUT,
        };
        if timeout.is_zero() {
            self.failed(&request);
            return None;
        }
        let write_selection = match &request {
            Request::Write { selection, .. } => Some(*selection),
            _ => None,
        };
        if let Some(selection) = write_selection {
            self.requests.retain(|queued| {
                !matches!(&queued.request, Request::Write { selection: target, .. } if *target == selection)
            });
            self.cancel_lists(selection);
        }
        if self.requests.len() >= MAX_REQUESTS {
            tracing::warn!("Clipboard request queue is full");
            self.failed(&request);
            return None;
        }
        let deadline = Instant::now() + timeout;
        self.requests.push_back(Queued { request, deadline });
        Some(deadline)
    }

    pub fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.active
            .as_ref()
            .map(|operation| operation.deadline)
            .into_iter()
            .chain(self.requests.iter().map(|queued| queued.deadline))
            .min()
    }

    fn expire(&mut self) {
        let now = Instant::now();
        if self
            .active
            .as_ref()
            .is_some_and(|active| now >= active.deadline)
        {
            let operation = self.active.take().unwrap();
            if !operation.cancelled {
                tracing::warn!(id = operation.id, "Terminal clipboard request timed out");
                self.failed(&operation.request);
            }
        }
        let mut requests = std::mem::take(&mut self.requests);
        requests.retain(|queued| {
            if now < queued.deadline {
                true
            } else {
                self.failed(&queued.request);
                false
            }
        });
        self.requests = requests;
    }

    pub fn poll(&mut self, output: &mut Vec<u8>) {
        self.expire();
        if self.active.is_some() || !output.is_empty() {
            return;
        }
        while let Some(Queued { request, deadline }) = self.requests.pop_front() {
            self.next += 1;
            let id = format!("meowland-{}", self.next);
            let mut metadata = format!(
                "id={id}:name={}:pw={}",
                STANDARD.encode("meowland"),
                STANDARD.encode(&self.password)
            );
            match &request {
                Request::List { selection, .. } => {
                    if *selection == Selection::Primary {
                        metadata.push_str(":loc=primary");
                    }
                    packet(output, &format!("type=read:{metadata}"), b".");
                }
                Request::Read {
                    request: number,
                    offer,
                    mime,
                    ..
                } => {
                    let Some(credentials) = self.offers.get_mut(offer) else {
                        self.actions.push_back(Action::Reply {
                            request: *number,
                            data: None,
                        });
                        continue;
                    };
                    if credentials.primary {
                        metadata.push_str(":loc=primary");
                    }
                    if let Some(password) = credentials.password.take() {
                        metadata = format!(
                            "id={id}:name={}:pw={}{}",
                            STANDARD.encode("Paste event"),
                            STANDARD.encode(password),
                            if credentials.primary {
                                ":loc=primary"
                            } else {
                                ""
                            }
                        );
                    }
                    packet(output, &format!("type=read:{metadata}"), mime.as_bytes());
                }
                Request::Write { selection, data } => {
                    if *selection == Selection::Primary {
                        metadata.push_str(":loc=primary");
                    }
                    packet(output, &format!("type=write:{metadata}"), b"");
                    for item in &data.items {
                        let metadata = format!("type=wdata:mime={}", STANDARD.encode(&item.mime));
                        if item.data.is_empty() {
                            packet(output, &metadata, b"");
                        }
                        for chunk in item.data.chunks(4095) {
                            packet(output, &metadata, chunk);
                        }
                    }
                    packet(output, "type=wdata", b"");
                }
            }
            self.active = Some(Operation {
                id,
                request,
                reading: Reading::new(),
                deadline,
                cancelled: false,
            });
            break;
        }
    }

    pub fn packet(&mut self, bytes: &[u8]) {
        self.expire();
        let Some(packet) = Packet::parse(bytes) else {
            return;
        };
        if packet.id.is_none() && packet.kind == "read" {
            if packet.status == "OK" && self.focused {
                self.notification = Some(Reading::new());
            }
            if let Some(reading) = &mut self.notification {
                match reading.accept(&packet, ".", MAX_TYPE_LIST) {
                    Some(true) => {
                        let reading = self.notification.take().unwrap();
                        let selection = if reading.credentials.primary {
                            Selection::Primary
                        } else {
                            Selection::Clipboard
                        };
                        self.cancel_lists(selection);
                        self.offer(reading, selection, true);
                        self.actions.push_back(Action::Unblock);
                    }
                    Some(false) => {}
                    None => {
                        self.notification = None;
                    }
                }
            }
            return;
        }
        let Some(operation) = &mut self.active else {
            return;
        };
        if packet.id != Some(operation.id.as_str()) {
            return;
        }
        let done = match &operation.request {
            Request::Write { .. } if packet.kind == "write" => match packet.status {
                "DONE" => Some(true),
                _ => None,
            },
            Request::List { .. } if packet.kind == "read" => {
                operation.reading.accept(&packet, ".", MAX_TYPE_LIST)
            }
            Request::Read { mime, .. } if packet.kind == "read" => {
                operation.reading.accept(&packet, mime, MAX_BYTES)
            }
            _ => None,
        };
        if done == Some(false) {
            return;
        }
        let operation = self.active.take().unwrap();
        if operation.cancelled {
            return;
        }
        if done.is_none() {
            tracing::warn!(
                status = packet.status,
                id = operation.id,
                "Terminal clipboard request failed"
            );
            self.failed(&operation.request);
            return;
        }
        match operation.request {
            Request::List {
                selection,
                shortcut,
                epoch,
            } if epoch == self.epoch(selection) => {
                self.offer(operation.reading, selection, false);
                if shortcut {
                    self.actions.push_back(Action::Unblock);
                }
            }
            Request::Read { request, .. } => self.actions.push_back(Action::Reply {
                request,
                data: Some(operation.reading.bytes),
            }),
            Request::List { shortcut, .. } => {
                if shortcut {
                    self.actions.push_back(Action::Unblock);
                }
            }
            Request::Write { .. } => {}
        }
    }

    fn offer(&mut self, reading: Reading, selection: Selection, paste: bool) {
        let Ok(types) = String::from_utf8(reading.bytes) else {
            return;
        };
        let mimes = mime_types(types.split_ascii_whitespace().map(str::to_owned));
        self.next += 1;
        let offer = self.next;
        if self.offers.len() >= MAX_OFFERS
            && let Some(oldest) = self.offers.keys().min().copied()
        {
            self.offers.remove(&oldest);
        }
        let mut credentials = reading.credentials;
        credentials.primary = selection == Selection::Primary;
        self.offers.insert(offer, credentials);
        self.actions.push_back(Action::Offer {
            selection,
            offer,
            mimes,
            paste,
        });
    }

    fn failed(&mut self, request: &Request) {
        match request {
            Request::List { shortcut: true, .. } => self.actions.push_back(Action::Unblock),
            Request::Read { request, .. } => self.actions.push_back(Action::Reply {
                request: *request,
                data: None,
            }),
            _ => {}
        }
    }
}

const fn selection_index(selection: Selection) -> usize {
    match selection {
        Selection::Clipboard => 0,
        Selection::Primary => 1,
    }
}

const fn request_selection(request: &Request) -> Selection {
    match request {
        Request::List { selection, .. }
        | Request::Read { selection, .. }
        | Request::Write { selection, .. } => *selection,
    }
}

fn packet(output: &mut Vec<u8>, metadata: &str, payload: &[u8]) {
    output.extend_from_slice(
        format!("\x1b]5522;{metadata};{}\x1b\\", STANDARD.encode(payload)).as_bytes(),
    );
}

struct Packet<'a> {
    kind: &'a str,
    status: &'a str,
    id: Option<&'a str>,
    mime: Option<String>,
    password: Option<String>,
    primary: bool,
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
            if decoded.len() > 4096 {
                return None;
            }
            String::from_utf8(decoded).ok()
        };
        Some(Self {
            kind: fields.get("type")?,
            status: fields.get("status")?,
            id: fields.get("id").copied(),
            mime: if fields.contains_key("mime") {
                Some(decode("mime")?)
            } else {
                None
            },
            password: if fields.contains_key("pw") {
                Some(decode("pw")?)
            } else {
                None
            },
            primary: fields.get("loc") == Some(&"primary"),
            payload,
        })
    }
}
