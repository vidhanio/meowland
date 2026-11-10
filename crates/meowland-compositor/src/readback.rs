//! Commit-blocked DMA-BUF acquire, submission, and readback completion.
//!
//! A DMA-BUF commit stays blocked until the GPU has copied its pixels into
//! a snapshot, so the commit applies with them.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex, Weak as SyncWeak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use calloop::{
    Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use meowland_core::pixels::FrameSize;
use meowland_loop::Sources;
use meowland_render::{BufferRelease, Readback, Snapshot, Submission, source_bound};
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

use super::{ClientState, Compositor};

mod fences;

const MAX_PENDING: usize = 32;
const MAX_PENDING_BYTES: usize = 256 * 1024 * 1024;
/// How long a client's write fences may keep its commit blocked.
///
/// The fences belong to the client, which can keep them unsignaled forever.
/// Past this deadline the readback is abandoned: the commit applies without
/// new pixels and the reservation returns to the budget.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

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

/// What a readback slot holds.
#[derive(Debug)]
enum Fill {
    /// The readback is still running and holds its budget.
    Pending(Reservation),
    /// The GPU produced pixels; the budget stays held until the commit
    /// that carries them applies.
    Ready {
        snapshot: Snapshot,
        _reservation: Reservation,
    },
    /// The readback ended without pixels and returned its budget.
    Empty,
}

#[derive(Debug)]
struct Slot {
    fill: Mutex<Fill>,
}

impl Slot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Fill> {
        self.fill.lock().expect("readback snapshot lock poisoned")
    }
}

/// The readback a surface commit carries.
#[derive(Default, Debug)]
enum PreparedSnapshot {
    /// The commit leaves the previous readback in place.
    #[default]
    Unchanged,
    /// The commit has no readback.
    Cleared,
    /// The commit's readback, filled once the GPU finishes.
    Prepared(Arc<Slot>),
}

impl Cacheable for PreparedSnapshot {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        std::mem::take(self)
    }

    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        if !matches!(self, Self::Unchanged) {
            *into = self;
        }
    }
}

/// Mark a surface as having no prepared readback snapshot.
pub(crate) fn clear(surface: &WlSurface) {
    with_states(surface, |states| {
        *states.cached_state.get::<PreparedSnapshot>().pending() = PreparedSnapshot::Cleared;
    });
}

/// Take the current readback's snapshot.
///
/// Returns `None` if the surface has no readback, or if the readback was
/// abandoned before the GPU produced pixels, so the commit applied without
/// them.
///
/// # Panics
/// Panics if a snapshot lock was poisoned by another thread panicking.
pub(crate) fn snapshot(surface: &WlSurface) -> Option<Snapshot> {
    with_states(surface, |states| {
        let prepared = std::mem::replace(
            states.cached_state.get::<PreparedSnapshot>().current(),
            PreparedSnapshot::Cleared,
        );
        let PreparedSnapshot::Prepared(slot) = prepared else {
            return None;
        };
        let fill = std::mem::replace(&mut *slot.lock(), Fill::Empty);
        match fill {
            Fill::Ready { snapshot, .. } => Some(snapshot),
            Fill::Pending(_) | Fill::Empty => None,
        }
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
///
/// Once any reader fails with GPU work still referencing the buffer, every
/// transaction using the buffer is cancelled: the client must not reuse a
/// buffer whose sampling cannot be shown to have finished.
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

/// GPU work one reader has outstanding on its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Nothing submitted; releasing the buffer is always safe.
    Waiting,
    /// Rendering and readback were submitted and may still sample the buffer.
    Submitted,
    /// The GPU finished with the buffer.
    Finished,
}

/// One reader's hold on a buffer, released when dropped.
struct BufferUse {
    readers: Arc<BufferReaders>,
    stage: Stage,
}

impl BufferUse {
    fn new(readers: Arc<BufferReaders>) -> Self {
        readers.pending.fetch_add(1, Ordering::AcqRel);
        Self {
            readers,
            stage: Stage::Waiting,
        }
    }
}

impl Drop for BufferUse {
    fn drop(&mut self) {
        if self.stage == Stage::Submitted {
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
    bound: Option<(u32, u32)>,
}

impl Drop for Operation {
    /// A readback that ends without filling its slot returns its budget
    /// now, even if the commit carrying the slot never applies.
    fn drop(&mut self) {
        let mut fill = self.slot.lock();
        if matches!(*fill, Fill::Pending(_)) {
            *fill = Fill::Empty;
        }
    }
}

/// A reader waiting for the buffer's writers before the GPU can sample it.
struct Acquiring {
    operation: Operation,
    dmabuf: Dmabuf,
    /// One registration per write fence; each is cleared once it signals.
    fences: Vec<Option<Sources<Compositor>>>,
    timeout: Sources<Compositor>,
}

/// A submitted GPU readback waiting for its completion fence.
struct Reading {
    operation: Operation,
    readback: Readback,
    completion: Sources<Compositor>,
}

enum Phase {
    Acquiring(Acquiring),
    Reading(Reading),
}

/// In-flight readbacks and buffer blockers.
#[derive(Default)]
pub(crate) struct Readbacks {
    next: u64,
    operations: HashMap<u64, Phase>,
    buffers: HashMap<ObjectId, SyncWeak<BufferReaders>>,
    budget: Arc<Budget>,
}

impl fmt::Debug for Readbacks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Readbacks")
            .field("operations", &self.operations.len())
            .field("buffers", &self.buffers.len())
            .finish_non_exhaustive()
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

impl Compositor {
    /// Let a client's blocked transactions apply once their blockers clear.
    fn notify_client(&mut self, client: &Client) {
        if let Some(client_state) = client.get_data::<ClientState>() {
            let display = self.display.clone();
            client_state
                .compositor_state
                .blocker_cleared(self, &display);
        }
    }
}

/// Start a commit-blocked readback for a DMA-BUF surface buffer.
pub(crate) fn prepare(
    host: &mut Compositor,
    surface: &WlSurface,
    buffer: &WlBuffer,
    dmabuf: &Dmabuf,
) {
    let Some(client) = surface.client() else {
        return;
    };
    let readers = host.readbacks.readers(buffer);
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
    let Some(reservation) = Reservation::new(&host.readbacks.budget, bytes) else {
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
    let slot = Arc::new(Slot {
        fill: Mutex::new(Fill::Pending(reservation)),
    });
    with_states(surface, |states| {
        *states.cached_state.get::<PreparedSnapshot>().pending() =
            PreparedSnapshot::Prepared(Arc::clone(&slot));
    });
    host.readbacks.next += 1;
    let id = host.readbacks.next;
    let operation = Operation {
        surface: surface.downgrade(),
        client,
        slot,
        buffer: BufferUse::new(readers),
        bound,
    };
    let fences = match fences::watch(host, id, dmabuf) {
        fences::Watch::Ready => {
            submit(host, id, operation, dmabuf);
            return;
        }
        fences::Watch::Pending(fences) => fences,
        fences::Watch::Failed => {
            let client = operation.client.clone();
            drop(operation);
            host.notify_client(&client);
            return;
        }
    };
    let mut timeout = Sources::new(&host.handle);
    match host
        .handle
        .insert_source(Timer::from_duration(ACQUIRE_TIMEOUT), move |_, (), host| {
            abandon(host, id);
            TimeoutAction::Drop
        }) {
        Ok(token) => timeout.timer = Some(token),
        Err(error) => {
            tracing::warn!(%error, "Cannot register DMA-BUF acquire timeout");
            let client = operation.client.clone();
            drop(operation);
            host.notify_client(&client);
            return;
        }
    }
    host.readbacks.operations.insert(
        id,
        Phase::Acquiring(Acquiring {
            operation,
            dmabuf: dmabuf.clone(),
            fences: fences.into_iter().map(Some).collect(),
            timeout,
        }),
    );
}

/// Record a signaled write fence, submitting once every fence signaled.
fn acquired(host: &mut Compositor, id: u64, index: usize) {
    let operations = &mut host.readbacks.operations;
    let Some(Phase::Acquiring(acquiring)) = operations.get_mut(&id) else {
        return;
    };
    if let Some(mut fence) = acquiring.fences.get_mut(index).and_then(Option::take) {
        fence.io = None;
    }
    if acquiring.fences.iter().any(Option::is_some) {
        return;
    }
    let Some(Phase::Acquiring(acquiring)) = operations.remove(&id) else {
        return;
    };
    let Acquiring {
        operation, dmabuf, ..
    } = acquiring;
    submit(host, id, operation, &dmabuf);
}

/// Give up on a readback whose write fences did not signal in time.
fn abandon(host: &mut Compositor, id: u64) {
    let operations = &mut host.readbacks.operations;
    let Some(Phase::Acquiring(mut acquiring)) = operations.remove(&id) else {
        return;
    };
    acquiring.timeout.timer = None;
    tracing::warn!("DMA-BUF write fences did not signal; retaining previous pixels");
    let client = acquiring.operation.client.clone();
    drop(acquiring);
    host.notify_client(&client);
}

fn submit(host: &mut Compositor, id: u64, mut operation: Operation, dmabuf: &Dmabuf) {
    if operation.surface.upgrade().is_err() {
        let client = operation.client.clone();
        drop(operation);
        host.notify_client(&client);
        return;
    }
    let Some(gpu) = host.gpu.as_mut() else {
        let client = operation.client.clone();
        drop(operation);
        host.notify_client(&client);
        return;
    };
    operation.buffer.stage = Stage::Submitted;
    match gpu.submit(dmabuf, operation.bound) {
        Ok(Submission { fence, readback }) => {
            match host
                .handle
                .insert_source(Generic::new(fence, Interest::READ, Mode::Level), {
                    move |_, fence, host| Ok(complete(host, id, fence))
                }) {
                Ok(token) => {
                    let mut completion = Sources::new(&host.handle);
                    completion.io = Some(token);
                    host.readbacks.operations.insert(
                        id,
                        Phase::Reading(Reading {
                            operation,
                            readback,
                            completion,
                        }),
                    );
                }
                Err(error) => {
                    tracing::error!(%error, "Cannot register GPU completion; stopping without releasing the buffer");
                    host.shutdown = true;
                }
            }
        }
        Err(error) => {
            let release = error.release();
            tracing::warn!(%error, ?release, "Cannot submit DMA-BUF readback");
            match release {
                BufferRelease::Safe => operation.buffer.stage = Stage::Finished,
                BufferRelease::Unsafe => host.shutdown = true,
            }
            let client = operation.client.clone();
            drop(operation);
            host.notify_client(&client);
        }
    }
}

fn complete(host: &mut Compositor, id: u64, fence: impl std::os::fd::AsFd) -> PostAction {
    let operations = &mut host.readbacks.operations;
    let Some(Phase::Reading(reading)) = operations.remove(&id) else {
        return PostAction::Remove;
    };
    let Reading {
        mut operation,
        readback,
        mut completion,
    } = reading;
    let readback = match readback.signaled(fence) {
        Ok(signaled) => signaled,
        Err(readback) => {
            operations.insert(
                id,
                Phase::Reading(Reading {
                    operation,
                    readback,
                    completion,
                }),
            );
            return PostAction::Continue;
        }
    };
    completion.io = None;
    operation.buffer.stage = Stage::Finished;
    if let Ok(surface) = operation.surface.upgrade()
        && let Some(gpu) = host.gpu.as_mut()
    {
        let mut spare = host.windows.surface_cache.take_spare(&surface);
        match gpu.complete(readback, &mut spare) {
            Ok(snapshot) => {
                let mut fill = operation.slot.lock();
                if let Fill::Pending(reservation) = std::mem::replace(&mut *fill, Fill::Empty) {
                    *fill = Fill::Ready {
                        snapshot,
                        _reservation: reservation,
                    };
                }
            }
            Err(error) => tracing::warn!(%error, "DMA-BUF readback failed"),
        }
        if let Some(spare) = spare {
            host.windows.surface_cache.return_spare(&surface, spare);
        }
    }
    let client = operation.client.clone();
    drop(operation);
    host.notify_client(&client);
    PostAction::Remove
}

/// Cancel acquisition for a destroyed surface.
///
/// Submitted reads stay registered until their native fence signals, because
/// dropping the reservation early would let the client reuse a buffer the GPU
/// still samples. Completion skips the slot when the surface is gone.
pub(crate) fn destroyed(host: &mut Compositor, surface: &WlSurface) {
    let surface = surface.downgrade();
    let mut clients = Vec::new();
    host.readbacks.operations.retain(|_, phase| match phase {
        Phase::Acquiring(acquiring) if acquiring.operation.surface == surface => {
            clients.push(acquiring.operation.client.clone());
            false
        }
        Phase::Acquiring(_) | Phase::Reading(_) => true,
    });
    for client in clients {
        host.notify_client(&client);
    }
}
