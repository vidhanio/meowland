//! Commit-blocked DMA-BUF acquire, submission, and readback completion.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak as SyncWeak};

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use meowland_core::pixels::FrameSize;
use meowland_render::{Readback, Submission, source_bound};
use smithay::backend::allocator::Buffer;
use smithay::backend::allocator::dmabuf::{AlreadyReady, Dmabuf, DmabufBlocker};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource, Weak};
use smithay::utils::Transform;
use smithay::wayland::compositor::{
    Blocker, BlockerState, Cacheable, SurfaceAttributes, add_blocker, with_states,
};

use super::sources::Sources;
use super::{ClientState, Snapshot, State, WlSurface};

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

#[derive(Debug)]
struct AcquireBlocker {
    blocker: DmabufBlocker,
    cancelled: Arc<AtomicBool>,
}

impl Blocker for AcquireBlocker {
    fn state(&self) -> BlockerState {
        if self.cancelled.load(Ordering::Acquire) {
            BlockerState::Released
        } else {
            self.blocker.state()
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

enum Phase {
    Acquiring(Dmabuf),
    Reading(Readback),
}

struct Operation {
    surface: Weak<WlSurface>,
    client: Client,
    slot: Arc<Slot>,
    buffer: BufferUse,
    acquire_cancelled: Arc<AtomicBool>,
    phase: Phase,
    sources: Sources,
    bound: Option<(u32, u32)>,
}

#[derive(Default)]
pub(crate) struct Readbacks {
    next: u64,
    operations: HashMap<u64, Operation>,
    buffers: HashMap<ObjectId, SyncWeak<BufferReaders>>,
    budget: Arc<Budget>,
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

pub(crate) fn prepare(state: &mut State, surface: &WlSurface, buffer: &WlBuffer, dmabuf: Dmabuf) {
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
        snapshot: Mutex::new(None),
        _reservation: reservation,
    });
    with_states(surface, |states| {
        states.cached_state.get::<PreparedSnapshot>().pending().slot = Some(Arc::clone(&slot));
    });
    state.readbacks.next += 1;
    let id = state.readbacks.next;
    let acquire = dmabuf.generate_blocker(Interest::READ);
    let acquire_cancelled = Arc::new(AtomicBool::new(false));
    state.readbacks.operations.insert(
        id,
        Operation {
            surface: surface.downgrade(),
            client,
            slot,
            buffer: BufferUse {
                readers,
                submitted: false,
                finished: false,
            },
            acquire_cancelled: Arc::clone(&acquire_cancelled),
            phase: Phase::Acquiring(dmabuf),
            sources: Sources::new(&state.handle),
            bound,
        },
    );
    match acquire {
        Ok((blocker, source)) => {
            match state.handle.insert_source(source, move |(), _, state| {
                if let Some(operation) = state.readbacks.operations.get_mut(&id) {
                    operation.sources.io = None;
                }
                submit(state, id);
                Ok(())
            }) {
                Ok(token) => {
                    state
                        .readbacks
                        .operations
                        .get_mut(&id)
                        .expect(
                            "operation remains registered while its acquire source is installed",
                        )
                        .sources
                        .io = Some(token);
                    add_blocker(
                        surface,
                        AcquireBlocker {
                            blocker,
                            cancelled: acquire_cancelled,
                        },
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "Cannot register DMA-BUF acquire readiness");
                    state.readbacks.operations.remove(&id);
                }
            }
        }
        Err(AlreadyReady) => submit(state, id),
    }
}

fn submit(state: &mut State, id: u64) {
    let Some(mut operation) = state.readbacks.operations.remove(&id) else {
        return;
    };
    if operation.surface.upgrade().is_err() {
        let client = operation.client.clone();
        drop(operation);
        notify(state, &client);
        return;
    }
    let Phase::Acquiring(dmabuf) = &operation.phase else {
        unreachable!();
    };
    let Some(gpu) = &mut state.gpu else {
        let client = operation.client.clone();
        drop(operation);
        notify(state, &client);
        return;
    };
    operation.buffer.submitted = true;
    let submission = gpu.submit(dmabuf, operation.bound);
    match submission {
        Ok(Submission { fd, readback }) => {
            operation.phase = Phase::Reading(readback);
            match state.handle.insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, state| {
                    complete(state, id);
                    Ok(PostAction::Remove)
                },
            ) {
                Ok(token) => {
                    operation.sources.io = Some(token);
                    state.readbacks.operations.insert(id, operation);
                }
                Err(error) => {
                    tracing::error!(%error, "Cannot register GPU completion; stopping without releasing the buffer");
                    state.shutdown = true;
                }
            }
        }
        Err(error) => {
            operation.buffer.finished = error.release_safe();
            tracing::warn!(%error, release_safe = error.release_safe(), "Cannot submit DMA-BUF readback");
            if !error.release_safe() {
                state.shutdown = true;
            }
            let client = operation.client.clone();
            drop(operation);
            notify(state, &client);
        }
    }
}

fn complete(state: &mut State, id: u64) {
    let Some(mut operation) = state.readbacks.operations.remove(&id) else {
        return;
    };
    operation.sources.io = None;
    let Phase::Reading(readback) = operation.phase else {
        unreachable!();
    };
    operation.buffer.finished = true;
    if let Some(gpu) = &mut state.gpu
        && operation.surface.upgrade().is_ok()
    {
        match gpu.complete(readback) {
            Ok(snapshot) => {
                *operation
                    .slot
                    .snapshot
                    .lock()
                    .expect("readback snapshot lock poisoned") = Some(snapshot);
            }
            Err(error) => tracing::warn!(%error, "DMA-BUF readback failed"),
        }
    }
    let client = operation.client;
    drop(operation.buffer);
    drop(operation.sources);
    notify(state, &client);
}

pub(crate) fn destroyed(state: &mut State, surface: &WlSurface) {
    let mut clients = Vec::new();
    state.readbacks.operations.retain(|_, operation| {
        if operation.surface == surface.downgrade()
            && matches!(operation.phase, Phase::Acquiring(_))
        {
            operation.acquire_cancelled.store(true, Ordering::Release);
            clients.push(operation.client.clone());
            false
        } else {
            true
        }
    });
    for client in clients {
        notify(state, &client);
    }
}

fn notify(state: &mut State, client: &Client) {
    if let Some(client_state) = client.get_data::<ClientState>() {
        let display = state.display.clone();
        client_state
            .compositor_state
            .blocker_cleared(state, &display);
    }
}
