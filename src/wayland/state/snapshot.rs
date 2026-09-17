//! Copying a client's buffer at the moment it commits it.

use std::collections::hash_map::Entry;

use smithay::{
    reexports::wayland_server::{Resource as _, protocol::wl_surface::WlSurface},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, with_states},
        viewporter::ensure_viewport_valid,
    },
};

use super::Compositor;
use crate::wayland::buffer::Snapshot;

/// Take a copy of the buffer a surface committed, and hand it straight back.
///
/// This is the moment client memory is read: from here on the surface is
/// composited from [`super::Compositor::snapshots`], so a client that
/// reuses its buffer cannot tear a frame.
impl Compositor {
    /// The biggest screen any pane has, which bounds the copy of a client
    /// buffer: nothing bigger can be shown.
    ///
    /// The capabilities are what the pane is, not what it currently holds: a
    /// frame the presenter has is a frame this cannot see.
    ///
    /// With no pane attached there is no bound, and the copy is the buffer's
    /// own size.
    fn snapshot_limit(&self) -> Option<(u32, u32)> {
        let (width, height) = self.views.iter().fold((0, 0), |largest, view| {
            (
                largest.0.max(view.capabilities.pixels.0),
                largest.1.max(view.capabilities.pixels.1),
            )
        });
        (width > 0 && height > 0).then_some((width, height))
    }

    pub(super) fn snapshot(&mut self, surface: &WlSurface) {
        let limit = self.snapshot_limit();

        let committed = with_states(surface, |states| {
            // The cached state is borrowed only for the reads that need it, so
            // the copy below holds no lock.
            let (buffer, scale) = {
                let mut state = states.cached_state.get::<SurfaceAttributes>();
                let current = state.current();
                let buffer = current.buffer.take();
                let scale = current.buffer_scale;
                // A snapshot copies the whole buffer, so all accumulated damage
                // is consumed. Left here it would make Smithay
                // keep every damage rectangle across later
                // commits.
                current.damage.clear();
                current.buffer_delta = None;
                drop(state);
                (buffer, scale)
            };
            buffer.map(|buffer| (buffer, scale))
        });
        match committed {
            Some((BufferAssignment::NewBuffer(buffer), scale)) => {
                let copied = match self.snapshots.entry(surface.id()) {
                    Entry::Occupied(mut entry) => crate::wayland::buffer::snapshot(
                        &buffer,
                        scale,
                        limit,
                        entry.get_mut(),
                        self.gpu.as_mut(),
                    ),
                    Entry::Vacant(entry) => {
                        let mut snapshot = Snapshot::empty();
                        let copied = crate::wayland::buffer::snapshot(
                            &buffer,
                            scale,
                            limit,
                            &mut snapshot,
                            self.gpu.as_mut(),
                        );
                        if copied {
                            entry.insert(snapshot);
                        }
                        copied
                    }
                };
                if !copied {
                    tracing::debug!(format = ?buffer, "buffer is not one we can composite");
                } else if let Some(snapshot) = self.snapshots.get(&surface.id()) {
                    let size = snapshot.logical_size();
                    with_states(surface, |states| {
                        ensure_viewport_valid(states, size.into());
                    });
                }
                buffer.release();
            }
            Some((BufferAssignment::Removed, _)) => {
                self.snapshots.remove(&surface.id());
            }
            None => {}
        }
    }
}
