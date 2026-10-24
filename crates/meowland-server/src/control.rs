//! Pending control operations, reply deadlines, and activation handoff.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use meowland_protocol::ControlResponse;
use tokio::sync::oneshot;

pub struct Reply {
    sender: oneshot::Sender<ControlResponse>,
    waiting: Arc<AtomicBool>,
    deadline: Instant,
}

impl Reply {
    pub const fn new(
        sender: oneshot::Sender<ControlResponse>,
        waiting: Arc<AtomicBool>,
        deadline: Instant,
    ) -> Self {
        Self {
            sender,
            waiting,
            deadline,
        }
    }

    pub fn waiting(&self) -> bool {
        self.waiting_at(Instant::now())
    }

    pub fn waiting_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.waiting)
    }

    fn waiting_at(&self, now: Instant) -> bool {
        self.waiting.load(Ordering::Relaxed) && !self.sender.is_closed() && now < self.deadline
    }

    pub fn send(self, response: ControlResponse) {
        self.waiting.store(false, Ordering::Relaxed);
        let _ = self.sender.send(response);
    }

    fn expire(self, reason: &str) {
        self.waiting.store(false, Ordering::Relaxed);
        let _ = self.sender.send(ControlResponse::Error(reason.into()));
    }
}

pub struct Launch {
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

pub enum Operation {
    Launch(Launch),
    Close,
}

pub struct PendingControl {
    pub operation: Operation,
    pub reply: Reply,
}

pub struct Begun {
    pub request: u64,
    pub deadline: Instant,
}

pub struct Requests {
    pending: HashMap<u64, PendingControl>,
    next: u64,
}

impl Requests {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next: 1,
        }
    }

    pub fn begin(&mut self, operation: Operation, reply: Reply) -> Begun {
        let request = self.next;
        self.next += 1;
        let deadline = reply.deadline;
        self.pending
            .insert(request, PendingControl { operation, reply });
        Begun { request, deadline }
    }

    pub fn finish(&mut self, request: u64, accepting: bool) -> Option<PendingControl> {
        let pending = self.pending.remove(&request)?;
        if accepting && pending.reply.waiting() {
            Some(pending)
        } else {
            pending.reply.expire("control request expired");
            None
        }
    }

    pub fn expire(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| !pending.reply.waiting_at(now))
            .map(|(request, _)| *request)
            .collect();
        for request in expired {
            if let Some(pending) = self.pending.remove(&request) {
                pending.reply.expire("compositor request timed out");
            }
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|pending| pending.reply.deadline)
            .min()
    }

    pub fn close_all(&mut self, reason: &str) {
        for (_, pending) in self.pending.drain() {
            pending.reply.expire(reason);
        }
    }

    pub fn abandon_all(&mut self) {
        for (_, pending) in self.pending.drain() {
            pending.reply.waiting.store(false, Ordering::Relaxed);
        }
    }
}
