//! Kitty OSC 5522 selection offers, permission-aware reads, and copy writes.

use std::{
    collections::{HashMap, VecDeque},
    io,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};

use super::output::Output;
use crate::clipboard::{ClipboardData, MAX_BYTES, MAX_TYPES, mime_types};

pub(super) enum Action {
    Offer {
        offer: u64,
        mimes: Vec<String>,
        paste: bool,
    },
    Reply {
        request: u64,
        data: Option<Vec<u8>>,
    },
    Unblock,
}

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
        shortcut: bool,
        epoch: u64,
    },
    Read {
        request: u64,
        offer: u64,
        mime: String,
    },
    Write(ClipboardData),
}

struct Operation {
    id: String,
    request: Request,
    reading: Reading,
    deadline: Instant,
}

pub(super) struct Clipboard {
    enabled: bool,
    password: String,
    focused: bool,
    epoch: u64,
    next: u64,
    requests: VecDeque<Request>,
    active: Option<Operation>,
    notification: Option<Reading>,
    offers: HashMap<u64, Credentials>,
    actions: VecDeque<Action>,
}

impl Clipboard {
    pub(super) fn new(enabled: bool) -> io::Result<Self> {
        let mut password = [0; 32];
        if enabled {
            getrandom::fill(&mut password).map_err(io::Error::other)?;
        }
        Ok(Self {
            enabled,
            password: STANDARD.encode(password),
            focused: true,
            epoch: 0,
            next: 0,
            requests: VecDeque::new(),
            active: None,
            notification: None,
            offers: HashMap::new(),
            actions: VecDeque::new(),
        })
    }

    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn focus(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.refresh(false);
        } else {
            self.epoch += 1;
            self.notification = None;
            self.requests
                .retain(|request| !matches!(request, Request::List { .. }));
        }
    }

    pub(super) fn refresh(&mut self, shortcut: bool) -> bool {
        if !self.enabled || (shortcut && (self.active.is_some() || !self.requests.is_empty())) {
            return false;
        }
        self.enqueue(Request::List {
            shortcut,
            epoch: self.epoch,
        });
        true
    }

    pub(super) fn read(&mut self, request: u64, offer: u64, mime: String) {
        self.enqueue(Request::Read {
            request,
            offer,
            mime,
        });
    }

    pub(super) fn copy(&mut self, data: ClipboardData, output: &mut Output) {
        if !data.valid() {
            return;
        }
        if self.enabled {
            self.enqueue(Request::Write(data));
        } else if let Some(text) = data.utf8_text() {
            output.append(format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text)).as_bytes());
        } else if data.items.is_empty() {
            output.append(b"\x1b]52;c;\x1b\\");
        }
    }

    fn enqueue(&mut self, request: Request) {
        if self.requests.len() >= 16 {
            tracing::warn!("Clipboard request queue is full");
            self.failed(&request);
        } else {
            if matches!(request, Request::Write(_)) {
                self.requests
                    .retain(|request| !matches!(request, Request::Write(_)));
            }
            self.requests.push_back(request);
        }
    }

    pub(super) fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub(super) fn poll(&mut self, output: &mut Output) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| Instant::now() >= active.deadline)
        {
            let operation = self.active.take().unwrap();
            tracing::warn!(id = operation.id, "Terminal clipboard request timed out");
            self.failed(&operation.request);
        }
        if self.active.is_some() || output.pending() {
            return;
        }
        while let Some(request) = self.requests.pop_front() {
            self.next += 1;
            let id = format!("meowland-{}", self.next);
            let mut metadata = format!(
                "id={id}:name={}:pw={}",
                STANDARD.encode("meowland"),
                STANDARD.encode(&self.password)
            );
            match &request {
                Request::List { .. } => packet(output, &format!("type=read:{metadata}"), b"."),
                Request::Read {
                    request: number,
                    offer,
                    mime,
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
                Request::Write(data) => {
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
            let timeout = if matches!(request, Request::List { .. }) {
                1
            } else {
                60
            };
            self.active = Some(Operation {
                id,
                request,
                reading: Reading::new(),
                deadline: Instant::now() + Duration::from_secs(timeout),
            });
            break;
        }
    }

    pub(super) fn packet(&mut self, bytes: &[u8]) {
        let Some(packet) = Packet::parse(bytes) else {
            return;
        };
        if packet.id.is_none() && packet.kind == "read" {
            if packet.status == "OK" && self.focused {
                self.notification = Some(Reading::new());
            }
            if let Some(reading) = &mut self.notification {
                match reading.accept(&packet, ".", MAX_TYPES * 257) {
                    Some(true) => {
                        let reading = self.notification.take().unwrap();
                        self.offer(reading, true);
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
            Request::Write(_) if packet.kind == "write" => match packet.status {
                "DONE" => Some(true),
                _ => None,
            },
            Request::List { .. } if packet.kind == "read" => {
                operation.reading.accept(&packet, ".", MAX_TYPES * 257)
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
            Request::List { shortcut, epoch } if epoch == self.epoch => {
                self.offer(operation.reading, false);
                if shortcut {
                    self.actions.push_back(Action::Unblock);
                }
            }
            Request::Read { request, .. } => self.actions.push_back(Action::Reply {
                request,
                data: Some(operation.reading.bytes),
            }),
            Request::List { .. } | Request::Write(_) => {}
        }
    }

    fn offer(&mut self, reading: Reading, paste: bool) {
        let Ok(types) = String::from_utf8(reading.bytes) else {
            return;
        };
        let mimes = mime_types(types.split_ascii_whitespace().map(str::to_owned));
        self.next += 1;
        let offer = self.next;
        if self.offers.len() >= 16
            && let Some(oldest) = self.offers.keys().min().copied()
        {
            self.offers.remove(&oldest);
        }
        self.offers.insert(offer, reading.credentials);
        self.actions.push_back(Action::Offer {
            offer,
            mimes,
            paste,
        });
    }

    fn failed(&mut self, request: &Request) {
        match request {
            Request::List {
                shortcut: true,
                epoch,
            } if *epoch == self.epoch => self.actions.push_back(Action::Unblock),
            Request::Read { request, .. } => self.actions.push_back(Action::Reply {
                request: *request,
                data: None,
            }),
            _ => {}
        }
    }
}

fn packet(output: &mut Output, metadata: &str, payload: &[u8]) {
    output.append(format!("\x1b]5522;{metadata};{}\x1b\\", STANDARD.encode(payload)).as_bytes());
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
