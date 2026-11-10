//! Commit-time snapshots of the implicit write fences on every DMA-BUF plane.

use std::os::fd::{FromRawFd as _, OwnedFd};

use calloop::{EventSource, Interest, Mode, PostAction, generic::Generic};
use meowland_loop::Sources;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    ioctl::{self, Opcode, Updater, opcode},
};
use smithay::backend::allocator::dmabuf::{AlreadyReady, Dmabuf};

use super::Compositor;

/// How a readback waits for the writers of its buffer.
pub(super) enum Watch {
    /// No writer is pending; the GPU may sample the buffer now.
    Ready,
    /// One registration per pending write fence or readiness source.
    Pending(Vec<Sources<Compositor>>),
    /// A wait could not be registered.
    Failed,
}

/// Register a wait for every pending writer of `dmabuf`, reporting each
/// signal to [`super::acquired`] with its index.
pub(super) fn watch(host: &Compositor, id: u64, dmabuf: &Dmabuf) -> Watch {
    let Some(fences) = snapshot(dmabuf) else {
        return match dmabuf.generate_blocker(Interest::READ) {
            Ok((_, source)) => register(host, source, move |(), _, host| {
                super::acquired(host, id, 0);
                Ok(())
            })
            .map_or(Watch::Failed, |sources| Watch::Pending(vec![sources])),
            Err(AlreadyReady) => Watch::Ready,
        };
    };
    if fences.is_empty() {
        return Watch::Ready;
    }
    let mut registered = Vec::with_capacity(fences.len());
    for (index, fence) in fences.into_iter().enumerate() {
        let source = Generic::new(fence, Interest::READ, Mode::Level);
        let Some(sources) = register(host, source, move |_, _, host| {
            super::acquired(host, id, index);
            Ok(PostAction::Remove)
        }) else {
            return Watch::Failed;
        };
        registered.push(sources);
    }
    Watch::Pending(registered)
}

fn register<S, F>(host: &Compositor, source: S, callback: F) -> Option<Sources<Compositor>>
where
    S: EventSource + 'static,
    F: FnMut(S::Event, &mut S::Metadata, &mut Compositor) -> S::Ret + 'static,
{
    match host.handle.insert_source(source, callback) {
        Ok(token) => {
            let mut sources = Sources::new(&host.handle);
            sources.io = Some(token);
            Some(sources)
        }
        Err(error) => {
            tracing::warn!(%error, "Cannot register DMA-BUF acquire wait");
            None
        }
    }
}

/// `dma_buf_export_sync_file` from the Linux DMA-BUF userspace ABI.
#[repr(C)]
struct ExportSyncFile {
    flags: u32,
    fd: i32,
}

/// A reader waits for the writers currently attached to the reservation object.
const DMA_BUF_SYNC_READ: u32 = 1;
/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` from `linux/dma-buf.h`.
const EXPORT_SYNC_FILE: Opcode = opcode::read_write::<ExportSyncFile>(b'b', 2);

/// Export independent sync files so later writes cannot extend this commit's
/// wait.
///
/// An unsupported ioctl or polling failure leaves the caller free to use the
/// DMA-BUF readiness fallback.
fn snapshot(dmabuf: &Dmabuf) -> Option<Vec<OwnedFd>> {
    let mut fences = Vec::new();
    for handle in dmabuf.handles() {
        let mut request = ExportSyncFile {
            flags: DMA_BUF_SYNC_READ,
            fd: -1,
        };
        // SAFETY: `EXPORT_SYNC_FILE` is the read-write `linux/dma-buf.h` opcode
        // for `ExportSyncFile`, whose `repr(C)` layout matches the kernel's
        // `struct dma_buf_export_sync_file`, and `request` outlives the
        // updater.
        let updater = unsafe { Updater::<EXPORT_SYNC_FILE, _>::new(&mut request) };
        // SAFETY: `handle` is a live DMA-BUF descriptor borrowed from `dmabuf`
        // and `updater` describes the request the kernel reads and writes.
        if let Err(error) = unsafe { ioctl::ioctl(handle, updater) } {
            tracing::debug!(%error, "cannot snapshot DMA-BUF write fences; polling DMA-BUF readiness");
            return None;
        }
        if request.fd < 0 {
            return None;
        }
        // SAFETY: A successful export stores a new nonnegative descriptor that
        // this function now exclusively owns.
        let fence = unsafe { OwnedFd::from_raw_fd(request.fd) };
        let mut pollfds = [PollFd::new(&fence, PollFlags::IN)];
        if poll(&mut pollfds, Some(&Timespec::default())).is_err()
            || pollfds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::NVAL)
        {
            return None;
        }
        if !pollfds[0].revents().contains(PollFlags::IN) {
            fences.push(fence);
        }
    }
    Some(fences)
}
