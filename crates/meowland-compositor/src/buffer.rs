//! Surface commits, copied snapshots, and buffer protocol handling.

use std::collections::HashMap;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    desktop::PopupManager,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_output, wl_surface::WlSurface},
    },
    utils::{Logical, Rectangle, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_pre_commit_hook, get_children, get_parent, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        shm::{ShmHandler, ShmState, with_buffer_contents},
        single_pixel_buffer::get_single_pixel_buffer,
        viewporter::ViewportCachedState,
    },
};

use super::{Snapshot, State, WindowId, readback, render, runtime::ClientState};

#[derive(Default)]
pub struct SurfaceCache {
    pub snapshots: HashMap<WlSurface, Snapshot>,
    geometry: HashMap<WlSurface, Geometry>,
    pub owners: HashMap<WlSurface, WindowId>,
}

impl SurfaceCache {
    fn remove(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
        self.geometry.remove(surface);
        self.owners.remove(surface);
    }

    pub fn remove_window(&mut self, id: WindowId) {
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

#[derive(Clone, Copy, PartialEq)]
pub struct Geometry {
    source: Option<Rectangle<f64, Logical>>,
    destination: Option<Size<i32, Logical>>,
    scale: i32,
    transform: wl_output::Transform,
}

impl Geometry {
    fn is_default(self) -> bool {
        self.source.is_none()
            && self.destination.is_none()
            && self.scale == 1
            && self.transform == wl_output::Transform::Normal
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
                        readback::prepare(state, surface, &buffer, dmabuf.clone());
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
        let (assignment, damaged, geometry, callbacks) = with_states(surface, |states| {
            let (assignment, damaged, scale, transform, callbacks) = {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let current = cached.current();
                let damaged = !current.damage.is_empty();
                current.damage.clear();
                (
                    current.buffer.take(),
                    damaged,
                    current.buffer_scale,
                    current.buffer_transform,
                    !current.frame_callbacks.is_empty(),
                )
            };
            let mut viewport = states.cached_state.get::<ViewportCachedState>();
            let current = viewport.current();
            (
                assignment,
                damaged,
                Geometry {
                    source: current.src,
                    destination: current.dst,
                    scale,
                    transform,
                },
                callbacks,
            )
        });
        if callbacks
            && let Some(window) = self
                .window_for_surface(surface)
                .and_then(|id| self.windows.get_mut(&id))
        {
            window.callback_pending = true;
        }
        let geometry_changed = if geometry.is_default() {
            self.surface_cache.geometry.remove(surface).is_some()
        } else {
            self.surface_cache
                .geometry
                .insert(surface.clone(), geometry)
                != Some(geometry)
        };
        let visual_change = assignment.is_some()
            || damaged
            || geometry_changed
            || get_parent(surface).is_some()
            || !get_children(surface).is_empty()
            || self.popups.find_popup(surface).is_some()
            || PopupManager::popups_for_surface(surface).next().is_some();
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = render::source_bound(
                    self.pane_bound(surface),
                    geometry.scale,
                    Transform::from(geometry.transform),
                );
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
            if visual_change {
                self.touch(window);
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
