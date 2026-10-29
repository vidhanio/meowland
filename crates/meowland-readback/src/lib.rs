//! Commit-blocked DMA-BUF acquire, submission, and readback completion.
//!
//! The state machine owns no compositor fields. A host supplies the GPU
//! backend, pane bounds, snapshot spares, client wakeups, and shutdown.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex, Weak as SyncWeak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use calloop::{Interest, Mode, PostAction, generic::Generic};
use meowland_core::{latency::Latency, pixels::FrameSize};
use meowland_loop::{Handle, Sources};
use meowland_render::{DmabufBackend, Readback, Snapshot, Submission, source_bound};
use smithay::{
    backend::allocator::{Buffer, dmabuf::Dmabuf},
    reexports::wayland_server::{
        Client, DisplayHandle, Resource, Weak,
        backend::ObjectId,
        protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    },
    utils::Transform,
    wayland::compositor::{
        Blocker, BlockerState, Cacheable, SurfaceAttributes, add_blocker, with_states,
    },
};

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
struct PreparedSnapshot {
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

/// Mark a surface as having no prepared readback snapshot.
pub fn clear(surface: &WlSurface) {
    with_states(surface, |states| {
        *states.cached_state.get::<PreparedSnapshot>().pending() = PreparedSnapshot {
            changed: true,
            slot: None,
        };
    });
}

/// When the current readback for a surface started acquiring, if any.
pub fn prepared_at(surface: &WlSurface) -> Option<Instant> {
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

/// Take the already-completed snapshot for a surface, if one is ready.
///
/// # Panics
/// Panics if a snapshot lock was poisoned by another thread panicking.
pub fn snapshot(surface: &WlSurface) -> Option<Snapshot> {
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
struct Operation<H> {
    surface: Weak<WlSurface>,
    client: Client,
    slot: Arc<Slot>,
    buffer: BufferUse,
    sources: Sources<H>,
    bound: Option<(u32, u32)>,
}

/// A reader waiting for the buffer's writers before the GPU can sample it.
struct Acquiring<H> {
    operation: Operation<H>,
    dmabuf: Dmabuf,
    acquire_sources: Vec<Sources<H>>,
    acquire_pending: usize,
}

/// A submitted GPU readback waiting for its completion fence.
struct Reading<H> {
    operation: Operation<H>,
    readback: Readback,
    submitted: Instant,
}

/// In-flight readbacks, buffer blockers, and latency accounting.
pub struct Readbacks<H> {
    next: u64,
    acquiring: HashMap<u64, Acquiring<H>>,
    reading: HashMap<u64, Reading<H>>,
    buffers: HashMap<ObjectId, SyncWeak<BufferReaders>>,
    budget: Arc<Budget>,
    stats: ReadbackStats,
}

impl<H> Default for Readbacks<H> {
    fn default() -> Self {
        Self {
            next: 0,
            acquiring: HashMap::new(),
            reading: HashMap::new(),
            buffers: HashMap::new(),
            budget: Arc::default(),
            stats: ReadbackStats::default(),
        }
    }
}

impl<H> fmt::Debug for Readbacks<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Readbacks")
            .field("acquiring", &self.acquiring.len())
            .field("reading", &self.reading.len())
            .field("buffers", &self.buffers.len())
            .finish_non_exhaustive()
    }
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

impl<H> Readbacks<H> {
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

/// Host operations the readback state machine needs.
pub trait Host: Sized + 'static {
    /// Event-loop projection for fences and completion readiness.
    fn handle(&self) -> &Handle<Self>;
    /// Host-owned readback state.
    fn readbacks(&mut self) -> &mut Readbacks<Self>;
    /// GPU backend that submits and completes readbacks.
    fn gpu(&mut self) -> Option<&mut DmabufBackend>;
    /// Logical buffer bound of the pane showing a surface.
    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)>;
    /// Take a snapshot buffer parked for reuse after a previous readback.
    fn take_spare(&mut self, surface: &WlSurface) -> Option<Snapshot>;
    /// Park a reused snapshot buffer after a failed readback.
    fn return_spare(&mut self, surface: WlSurface, snapshot: Snapshot);
    /// Notify a client that its buffer blockers were released.
    fn notify_client(&mut self, client: &Client);
    /// Stop the compositor because a readback cannot safely release its buffer.
    fn request_shutdown(&mut self);
}

/// Start a commit-blocked readback for a DMA-BUF surface buffer.
pub fn prepare<H: Host>(host: &mut H, surface: &WlSurface, buffer: &WlBuffer, dmabuf: &Dmabuf) {
    let started = Instant::now();
    let Some(client) = surface.client() else {
        return;
    };
    let readers = host.readbacks().readers(buffer);
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
    let Some(reservation) = Reservation::new(&host.readbacks().budget, bytes) else {
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
    let bound = source_bound(host.pane_bound(surface), scale, transform);
    readers.pending.fetch_add(1, Ordering::AcqRel);
    let slot = Arc::new(Slot {
        started,
        snapshot: Mutex::new(None),
        _reservation: reservation,
    });
    with_states(surface, |states| {
        states.cached_state.get::<PreparedSnapshot>().pending().slot = Some(Arc::clone(&slot));
    });
    host.readbacks().next += 1;
    let id = host.readbacks().next;
    let sources = Sources::new(host.handle());
    host.readbacks().acquiring.insert(
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
                sources,
                bound,
            },
            dmabuf: dmabuf.clone(),
            acquire_sources: Vec::new(),
            acquire_pending: 0,
        },
    );
    fences::watch(host, id, dmabuf);
}

fn submit<H: Host>(host: &mut H, id: u64) {
    let Some(mut acquiring) = host.readbacks().acquiring.remove(&id) else {
        return;
    };
    if acquiring.operation.surface.upgrade().is_err() {
        let client = acquiring.operation.client.clone();
        drop(acquiring);
        host.notify_client(&client);
        return;
    }
    acquiring.acquire_sources.clear();
    let Some(gpu) = host.gpu() else {
        let client = acquiring.operation.client.clone();
        drop(acquiring);
        host.notify_client(&client);
        return;
    };
    let submitted = Instant::now();
    acquiring.operation.buffer.submitted = true;
    match gpu.submit(&acquiring.dmabuf, acquiring.operation.bound) {
        Ok(Submission { fd, readback }) => {
            match host
                .handle()
                .insert_source(Generic::new(fd, Interest::READ, Mode::Level), {
                    move |_, _, host| {
                        complete(host, id);
                        Ok(PostAction::Remove)
                    }
                }) {
                Ok(token) => {
                    let Acquiring { mut operation, .. } = acquiring;
                    operation.sources.io = Some(token);
                    host.readbacks().reading.insert(
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
                    host.request_shutdown();
                }
            }
        }
        Err(error) => {
            acquiring.operation.buffer.finished = error.release_safe();
            tracing::warn!(
                %error,
                release_safe = error.release_safe(),
                "Cannot submit DMA-BUF readback"
            );
            if !error.release_safe() {
                host.request_shutdown();
            }
            let client = acquiring.operation.client.clone();
            drop(acquiring);
            host.notify_client(&client);
        }
    }
}

fn complete<H: Host>(host: &mut H, id: u64) {
    let Some(Reading {
        mut operation,
        readback,
        submitted,
    }) = host.readbacks().reading.remove(&id)
    else {
        return;
    };
    operation.sources.io = None;
    let ready = Instant::now();
    operation.buffer.finished = true;
    if let Ok(surface) = operation.surface.upgrade() {
        let mut previous = host.take_spare(&surface);
        if let Some(gpu) = host.gpu() {
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
                        host.return_spare(surface, previous);
                    }
                    tracing::warn!(%error, "DMA-BUF readback failed");
                }
            }
        } else if let Some(previous) = previous {
            host.return_spare(surface, previous);
        }
    }
    host.readbacks()
        .stats
        .complete(operation.slot.started, submitted, ready);
    let client = operation.client;
    drop(operation.buffer);
    drop(operation.sources);
    host.notify_client(&client);
}

/// Cancel acquisition for a destroyed surface.
///
/// Submitted reads stay registered until their native fence signals, because
/// dropping the reservation early would let the client reuse a buffer the GPU
/// still samples. Completion skips the slot when the surface is gone.
pub fn destroyed<H: Host>(host: &mut H, surface: &WlSurface) {
    let mut clients = Vec::new();
    host.readbacks().acquiring.retain(|_, acquiring| {
        if acquiring.operation.surface == surface.downgrade() {
            clients.push(acquiring.operation.client.clone());
            false
        } else {
            true
        }
    });
    for client in clients {
        host.notify_client(&client);
    }
}
