//! Surface commits, copied snapshots, and buffer protocol handling.

use std::{collections::HashMap, time::Instant};

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    utils::Transform,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_pre_commit_hook, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        shm::{ShmHandler, ShmState, with_buffer_contents},
        single_pixel_buffer::get_single_pixel_buffer,
    },
};

use super::{Snapshot, State, WindowId, readback, render, runtime::ClientState};

#[derive(Default)]
pub(crate) struct SurfaceCache {
    pub snapshots: HashMap<WlSurface, Snapshot>,
    pub owners: HashMap<WlSurface, WindowId>,
}

impl SurfaceCache {
    fn remove(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
        self.owners.remove(surface);
    }

    pub(crate) fn remove_window(&mut self, id: WindowId) {
        let surfaces: Vec<_> = self
            .owners
            .iter()
            .filter_map(|(surface, &owner)| (owner == id).then_some(surface.clone()))
            .collect();
        for surface in surfaces {
            self.remove(&surface);
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.gpu.as_mut().is_some_and(|gpu| gpu.validate(&dmabuf)) {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.surfaces
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client state")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let assignment = with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                match &attributes.pending().buffer {
                    Some(BufferAssignment::NewBuffer(buffer)) => {
                        Some(BufferAssignment::NewBuffer(buffer.clone()))
                    }
                    Some(BufferAssignment::Removed) => Some(BufferAssignment::Removed),
                    None => None,
                }
            });
            match assignment {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    if let Ok(dmabuf) = get_dmabuf(&buffer) {
                        readback::prepare(state, surface, &buffer, dmabuf);
                    } else {
                        readback::clear(surface);
                    }
                }
                Some(BufferAssignment::Removed) => readback::clear(surface),
                None => {}
            }
        });
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "`cached` borrows from the guard and must outlive `current`"
    )]
    fn commit(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);
        if let Some(window) = self.window_for_surface(surface) {
            self.surface_cache.owners.insert(surface.clone(), window);
        }
        let changed = readback::prepared_at(surface).unwrap_or_else(Instant::now);
        let (assignment, damage, scale, transform, callbacks) = with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceAttributes>();
            let current = cached.current();
            (
                current.buffer.take(),
                std::mem::take(&mut current.damage),
                current.buffer_scale,
                Transform::from(current.buffer_transform),
                !current.frame_callbacks.is_empty(),
            )
        });
        let new_buffer = matches!(assignment, Some(BufferAssignment::NewBuffer(_)));
        if callbacks
            && let Some(window) = self
                .window_for_surface(surface)
                .and_then(|id| self.windows.get_mut(&id))
        {
            window.callback_pending = true;
        }
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = render::source_bound(self.pane_bound(surface), scale, transform);
                let mut previous = self.surface_cache.snapshots.remove(surface);
                let single_pixel = get_single_pixel_buffer(&buffer)
                    .ok()
                    .map(|pixel| Snapshot::single_pixel(pixel.rgba8888()));
                let snapshot = single_pixel.or_else(|| {
                    if get_dmabuf(&buffer).is_ok() {
                        readback::snapshot(surface)
                    } else {
                        with_buffer_contents(&buffer, |ptr, len, data| {
                            // SAFETY: Smithay keeps the mapped pool readable
                            // for this callback and
                            // supplies its pointer and length
                            // together.
                            unsafe { Snapshot::copy_shm(ptr, len, &data, bound, &mut previous) }
                        })
                        .ok()
                        .flatten()
                    }
                });
                buffer.release();
                if let Some(snapshot) = snapshot.or(previous) {
                    self.surface_cache
                        .snapshots
                        .insert(surface.clone(), snapshot);
                }
            }
            Some(BufferAssignment::Removed) => {
                self.surface_cache.snapshots.remove(surface);
            }
            None => {}
        }
        self.refresh_window_mapping(surface);
        if let Some(window) = self.window_for_surface(surface) {
            if self.attachments.visible(window) && self.catalog.drawable(window) {
                self.output.enter(surface);
            }
            let damage = self
                .surface_cache
                .snapshots
                .get(surface)
                .map_or_default(|snapshot| {
                    let geometry = render::surface_geometry(surface, snapshot);
                    if new_buffer && damage.is_empty() {
                        render::SurfaceDamage::full(&geometry)
                    } else {
                        render::SurfaceDamage::new(&damage, &geometry)
                    }
                });
            for entry in self.attachments.panes.values_mut() {
                if entry.window() == Some(window) {
                    entry.presentation.damage(surface, damage, changed);
                }
            }
        }
    }

    /// A destroyed surface may not belong to any toplevel, so remove its
    /// snapshot here rather than relying on toplevel cleanup.
    fn destroyed(&mut self, surface: &WlSurface) {
        let window = self.window_for_surface(surface);
        self.surface_cache.remove(surface);
        readback::destroyed(self, surface);
        if let Some(window) = window {
            if self.ids.get(surface) == Some(&window) {
                self.unmap_window(window);
            } else {
                self.touch(window);
            }
        }
    }
}
