//! Commit-time snapshots of the implicit write fences on every DMA-BUF plane.

use std::os::fd::{FromRawFd as _, OwnedFd};

use calloop::{Interest, Mode, PostAction, RegistrationToken, generic::Generic};
use meowland_loop::Sources;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    ioctl::{self, Opcode, Updater, opcode},
};
use smithay::backend::allocator::dmabuf::{AlreadyReady, Dmabuf};

use super::Host;

pub(super) fn watch<H: Host>(host: &mut H, id: u64, dmabuf: &Dmabuf) {
    if let Some(fences) = snapshot(dmabuf) {
        host.readbacks()
            .acquiring
            .get_mut(&id)
            .expect("operation exists during acquire setup")
            .acquire_pending = fences.len();
        if fences.is_empty() {
            super::submit(host, id);
            return;
        }
        for (index, fence) in fences.into_iter().enumerate() {
            let source = Generic::new(fence, Interest::READ, Mode::Level);
            match host.handle().insert_source(source, move |_, _, host| {
                acquired(host, id, index);
                Ok(PostAction::Remove)
            }) {
                Ok(token) => add_source(host, id, token),
                Err(error) => {
                    tracing::warn!(%error, "Cannot register DMA-BUF acquire fence");
                    abandon(host, id);
                    break;
                }
            }
        }
    } else {
        host.readbacks()
            .acquiring
            .get_mut(&id)
            .expect("operation exists during acquire setup")
            .acquire_pending = 1;
        match dmabuf.generate_blocker(Interest::READ) {
            Ok((_, source)) => match host.handle().insert_source(source, move |(), _, host| {
                acquired(host, id, 0);
                Ok(())
            }) {
                Ok(token) => add_source(host, id, token),
                Err(error) => {
                    tracing::warn!(%error, "Cannot register DMA-BUF acquire readiness");
                    abandon(host, id);
                }
            },
            Err(AlreadyReady) => super::submit(host, id),
        }
    }
}

fn add_source<H: Host>(host: &mut H, id: u64, token: RegistrationToken) {
    let mut sources = Sources::new(host.handle());
    sources.io = Some(token);
    host.readbacks()
        .acquiring
        .get_mut(&id)
        .expect("operation remains registered during acquire setup")
        .acquire_sources
        .push(sources);
}

fn acquired<H: Host>(host: &mut H, id: u64, index: usize) {
    let Some(acquiring) = host.readbacks().acquiring.get_mut(&id) else {
        return;
    };
    acquiring.acquire_sources[index].io = None;
    acquiring.acquire_pending -= 1;
    if acquiring.acquire_pending == 0 {
        super::submit(host, id);
    }
}

fn abandon<H: Host>(host: &mut H, id: u64) {
    if let Some(acquiring) = host.readbacks().acquiring.remove(&id) {
        let client = acquiring.operation.client.clone();
        drop(acquiring);
        host.notify_client(&client);
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
/// DMA-BUF readiness fallback. Successful ioctl results transfer ownership of
/// a new descriptor to this function; the request matches `linux/dma-buf.h`.
#[expect(
    clippy::undocumented_unsafe_blocks,
    reason = "the ioctl ABI and returned descriptor ownership are documented above"
)]
fn snapshot(dmabuf: &Dmabuf) -> Option<Vec<OwnedFd>> {
    let mut fences = Vec::new();
    for handle in dmabuf.handles() {
        let mut request = ExportSyncFile {
            flags: DMA_BUF_SYNC_READ,
            fd: -1,
        };
        let updater = unsafe { Updater::<EXPORT_SYNC_FILE, _>::new(&mut request) };
        if let Err(error) = unsafe { ioctl::ioctl(handle, updater) } {
            tracing::debug!(%error, "cannot snapshot DMA-BUF write fences; polling DMA-BUF readiness");
            return None;
        }
        if request.fd < 0 {
            return None;
        }
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
