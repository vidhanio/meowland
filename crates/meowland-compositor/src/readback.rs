//! Commit-blocked DMA-BUF acquire, submission, and readback completion.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak as SyncWeak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use calloop::{Interest, Mode, PostAction, generic::Generic};
use meowland_core::{latency::Latency, pixels::FrameSize};
use meowland_render::{Readback, Submission, source_bound};
use smithay::{
    backend::allocator::{Buffer, dmabuf::Dmabuf},
    reexports::wayland_server::{
        Client, DisplayHandle, Resource, Weak, backend::ObjectId, protocol::wl_buffer::WlBuffer,
    },
    utils::Transform,
    wayland::compositor::{
        Blocker, BlockerState, Cacheable, SurfaceAttributes, add_blocker, with_states,
    },
};

use super::{ClientState, Compositor, Snapshot, Sources, WlSurface};

mod fences;

const MAX_PENDING: usize = 32;
const MAX_PENDING_BYTES: usize = 256 * 1024 * 1024;

#[derive(Default, Debug)]
struct Budget {
    pending: AtomicUsize,
    bytes: AtomicUsize,
}

#[derive(Debug)]
struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Reservation {
    fn new(budget: &Arc<Budget>, bytes: usize) -> Option<Self> {
        budget
            .pending
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                pending
                    .checked_add(1)
                    .filter(|&pending| pending <= MAX_PENDING)
            })
            .ok()?;
        if budget
            .bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                total
                    .checked_add(bytes)
                    .filter(|&total| total <= MAX_PENDING_BYTES)
            })
            .is_err()
        {
            budget.pending.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self {
            budget: Arc::clone(budget),
            bytes,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
        self.budget.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Slot {
    started: Instant,
    snapshot: Mutex<Option<Snapshot>>,
    _reservation: Reservation,
}

#[derive(Default, Debug)]
pub(crate) struct PreparedSnapshot {
    changed: bool,
    slot: Option<Arc<Slot>>,
}

impl Cacheable for PreparedSnapshot {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        std::mem::take(self)
    }

    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        if self.changed {
            *into = self;
        }
    }
}

pub(crate) fn clear(surface: &WlSurface) {
    with_states(surface, |states| {
        *states.cached_state.get::<PreparedSnapshot>().pending() = PreparedSnapshot {
            changed: true,
            slot: None,
        };
    });
}

pub(crate) fn prepared_at(surface: &WlSurface) -> Option<Instant> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<PreparedSnapshot>()
            .current()
            .slot
            .as_ref()
            .map(|slot| slot.started)
    })
}

pub(crate) fn snapshot(surface: &WlSurface) -> Option<Snapshot> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<PreparedSnapshot>()
            .current()
            .slot
            .take()
            .and_then(|slot| {
                slot.snapshot
                    .lock()
                    .expect("readback snapshot lock poisoned")
                    .take()
            })
    })
}

#[derive(Default, Debug)]
struct BufferReaders {
    pending: AtomicUsize,
    failed: AtomicBool,
}

/// Gate every transaction using a buffer until all its submitted readers
/// finish, including readers on other surfaces that Smithay might otherwise
/// supersede.
#[derive(Clone, Debug)]
struct BufferBlocker(Arc<BufferReaders>);

impl Blocker for BufferBlocker {
    fn state(&self) -> BlockerState {
        if self.0.failed.load(Ordering::Acquire) {
            BlockerState::Cancelled
        } else if self.0.pending.load(Ordering::Acquire) == 0 {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

struct BufferUse {
    readers: Arc<BufferReaders>,
    submitted: bool,
    finished: bool,
}

impl Drop for BufferUse {
    fn drop(&mut self) {
        if self.submitted && !self.finished {
            self.readers.failed.store(true, Ordering::Release);
        }
        self.readers.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bookkeeping for one buffer that must stay alive until its readback ends.
struct Operation {
    surface: Weak<WlSurface>,
    client: Client,
    slot: Arc<Slot>,
    buffer: BufferUse,
    sources: Sources,
    bound: Option<(u32, u32)>,
}

/// A reader waiting for the buffer's writers before the GPU can sample it.
struct Acquiring {
    operation: Operation,
    dmabuf: Dmabuf,
    acquire_sources: Vec<Sources>,
    acquire_pending: usize,
}

/// A submitted GPU readback waiting for its completion fence.
struct Reading {
    operation: Operation,
    readback: Readback,
    submitted: Instant,
}

#[derive(Default)]
pub(crate) struct Readbacks {
    next: u64,
    acquiring: HashMap<u64, Acquiring>,
    reading: HashMap<u64, Reading>,
    buffers: HashMap<ObjectId, SyncWeak<BufferReaders>>,
    budget: Arc<Budget>,
    stats: ReadbackStats,
}

struct ReadbackStats {
    window: Instant,
    completed: u32,
    acquire: Latency,
    readback: Latency,
    copy: Latency,
}

impl Default for ReadbackStats {
    fn default() -> Self {
        Self {
            window: Instant::now(),
            completed: 0,
            acquire: Latency::default(),
            readback: Latency::default(),
            copy: Latency::default(),
        }
    }
}

impl ReadbackStats {
    fn complete(&mut self, started: Instant, submitted: Instant, ready: Instant) {
        self.acquire
            .record(submitted.saturating_duration_since(started));
        self.readback
            .record(ready.saturating_duration_since(submitted));
        self.copy.record(ready.elapsed());
        self.completed += 1;
        if self.window.elapsed() >= Duration::from_secs(1) {
            tracing::debug!(
                completed = self.completed,
                acquire_us = ?self.acquire.summarize(),
                readback_us = ?self.readback.summarize(),
                copy_us = ?self.copy.summarize(),
                "DMA-BUF latency"
            );
            self.completed = 0;
            self.window = Instant::now();
        }
    }
}

impl Readbacks {
    fn readers(&mut self, buffer: &WlBuffer) -> Arc<BufferReaders> {
        self.buffers.retain(|_, readers| readers.strong_count() > 0);
        self.buffers
            .get(&buffer.id())
            .and_then(SyncWeak::upgrade)
            .unwrap_or_else(|| {
                let readers = Arc::new(BufferReaders::default());
                self.buffers.insert(buffer.id(), Arc::downgrade(&readers));
                readers
            })
    }
}

pub(crate) fn prepare(
    state: &mut Compositor,
    surface: &WlSurface,
    buffer: &WlBuffer,
    dmabuf: &Dmabuf,
) {
    let started = Instant::now();
    let Some(client) = surface.client() else {
        return;
    };
    let readers = state.readbacks.readers(buffer);
    add_blocker(surface, BufferBlocker(Arc::clone(&readers)));
    clear(surface);
    let size = dmabuf.size();
    let size = u32::try_from(size.w)
        .ok()
        .zip(u32::try_from(size.h).ok())
        .and_then(|(width, height)| FrameSize::new(width, height));
    let Some(size) = size else {
        return;
    };
    let bytes = size.rgba_len();
    let Some(reservation) = Reservation::new(&state.readbacks.budget, bytes) else {
        tracing::warn!("DMA-BUF readback budget exhausted; retaining previous pixels");
        return;
    };
    let (scale, transform) = with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        let pending = cached.pending();
        let properties = (
            pending.buffer_scale,
            Transform::from(pending.buffer_transform),
        );
        drop(cached);
        properties
    });
    let bound = source_bound(state.pane_bound(surface), scale, transform);
    readers.pending.fetch_add(1, Ordering::AcqRel);
    let slot = Arc::new(Slot {
        started,
        snapshot: Mutex::new(None),
        _reservation: reservation,
    });
    with_states(surface, |states| {
        states.cached_state.get::<PreparedSnapshot>().pending().slot = Some(Arc::clone(&slot));
    });
    state.readbacks.next += 1;
    let id = state.readbacks.next;
    state.readbacks.acquiring.insert(
        id,
        Acquiring {
            operation: Operation {
                surface: surface.downgrade(),
                client,
                slot,
                buffer: BufferUse {
                    readers,
                    submitted: false,
                    finished: false,
                },
                sources: Sources::new(&state.handle),
                bound,
            },
            dmabuf: dmabuf.clone(),
            acquire_sources: Vec::new(),
            acquire_pending: 0,
        },
    );
    fences::watch(state, id, dmabuf);
}

fn submit(state: &mut Compositor, id: u64) {
    let Some(mut acquiring) = state.readbacks.acquiring.remove(&id) else {
        return;
    };
    if acquiring.operation.surface.upgrade().is_err() {
        let client = acquiring.operation.client.clone();
        drop(acquiring);
        notify(state, &client);
        return;
    }
    acquiring.acquire_sources.clear();
    let Some(gpu) = &mut state.gpu else {
        let client = acquiring.operation.client.clone();
        drop(acquiring);
        notify(state, &client);
        return;
    };
    let submitted = Instant::now();
    acquiring.operation.buffer.submitted = true;
    match gpu.submit(&acquiring.dmabuf, acquiring.operation.bound) {
        Ok(Submission { fd, readback }) => {
            match state.handle.insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, state| {
                    complete(state, id);
                    Ok(PostAction::Remove)
                },
            ) {
                Ok(token) => {
                    let Acquiring { mut operation, .. } = acquiring;
                    operation.sources.io = Some(token);
                    state.readbacks.reading.insert(
                        id,
                        Reading {
                            operation,
                            readback,
                            submitted,
                        },
                    );
                }
                Err(error) => {
                    tracing::error!(%error, "Cannot register GPU completion; stopping without releasing the buffer");
                    state.shutdown = true;
                }
            }
        }
        Err(error) => {
            acquiring.operation.buffer.finished = error.release_safe();
            tracing::warn!(%error, release_safe = error.release_safe(), "Cannot submit DMA-BUF readback");
            if !error.release_safe() {
                state.shutdown = true;
            }
            let client = acquiring.operation.client.clone();
            drop(acquiring);
            notify(state, &client);
        }
    }
}

fn complete(state: &mut Compositor, id: u64) {
    let Some(Reading {
        mut operation,
        readback,
        submitted,
    }) = state.readbacks.reading.remove(&id)
    else {
        return;
    };
    operation.sources.io = None;
    let ready = Instant::now();
    operation.buffer.finished = true;
    if let Some(gpu) = &mut state.gpu
        && let Ok(surface) = operation.surface.upgrade()
    {
        let mut previous = state.windows.surface_cache.spares.remove(&surface);
        match gpu.complete(readback, &mut previous) {
            Ok(snapshot) => {
                *operation
                    .slot
                    .snapshot
                    .lock()
                    .expect("readback snapshot lock poisoned") = Some(snapshot);
            }
            Err(error) => {
                if let Some(previous) = previous {
                    state.windows.surface_cache.spares.insert(surface, previous);
                }
                tracing::warn!(%error, "DMA-BUF readback failed");
            }
        }
    }
    state
        .readbacks
        .stats
        .complete(operation.slot.started, submitted, ready);
    let client = operation.client;
    drop(operation.buffer);
    drop(operation.sources);
    notify(state, &client);
}

/// Cancel acquisition for a destroyed surface.
///
/// Submitted reads are left in [`Readbacks::reading`] until their native
/// fence signals, because dropping the reservation early would let the client
/// reuse a buffer the GPU still samples. Completion skips the slot when the
/// surface is gone.
pub(crate) fn destroyed(state: &mut Compositor, surface: &WlSurface) {
    let mut clients = Vec::new();
    state.readbacks.acquiring.retain(|_, acquiring| {
        if acquiring.operation.surface == surface.downgrade() {
            clients.push(acquiring.operation.client.clone());
            false
        } else {
            true
        }
    });
    for client in clients {
        notify(state, &client);
    }
}

fn notify(state: &mut Compositor, client: &Client) {
    if let Some(client_state) = client.get_data::<ClientState>() {
        let display = state.display.clone();
        client_state
            .compositor_state
            .blocker_cleared(state, &display);
    }
}
