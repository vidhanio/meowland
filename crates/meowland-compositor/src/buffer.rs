//! Surface commits, copied snapshots, and buffer protocol handling.

use std::{collections::HashMap, time::Instant};

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    utils::{Buffer, Rectangle, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage,
            SurfaceAttributes, add_pre_commit_hook, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        shm::{ShmHandler, ShmState, with_buffer_contents},
        single_pixel_buffer::get_single_pixel_buffer,
        viewporter::ViewportCachedState,
    },
};

use super::{Compositor, Snapshot, readback, render, runtime::ClientState};

#[derive(Default)]
pub(crate) struct SurfaceCache {
    pub snapshots: HashMap<WlSurface, Snapshot>,
    pub spares: HashMap<WlSurface, Snapshot>,
}

impl SurfaceCache {
    pub(crate) fn forget(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
        self.spares.remove(surface);
    }
}

/// Translate committed surface damage into buffer coordinates for snapshot
/// patching.
///
/// Empty damage means the whole buffer may have changed. A viewport maps
/// surface damage through a source region, so patching falls back to a full
/// copy for those surfaces.
fn buffer_damage(
    damage: &[Damage],
    data: &smithay::wayland::shm::BufferData,
    scale: i32,
    transform: Transform,
    viewport: ViewportCachedState,
) -> Vec<Rectangle<i32, Buffer>> {
    if damage.is_empty() {
        return Vec::new();
    }
    let size = Size::<i32, Buffer>::from((data.width, data.height));
    let surface = size.to_logical(scale, transform);
    let mut rects = Vec::with_capacity(damage.len());
    for damage in damage {
        let rect = match damage {
            Damage::Buffer(rect) => *rect,
            Damage::Surface(rect) => {
                if viewport.src.is_some() || viewport.dst.is_some() {
                    return Vec::new();
                }
                rect.to_buffer(scale, transform, &surface)
            }
        };
        rects.push(rect);
    }
    rects
}

impl BufferHandler for Compositor {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Compositor {
    fn shm_state(&self) -> &ShmState {
        &self.protocols.shm
    }
}

impl DmabufHandler for Compositor {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.protocols.dmabuf
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

impl CompositorHandler for Compositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.protocols.surfaces
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
        self.windows.popups.commit(surface);
        let window = self.window_for_surface(surface);
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
        if callbacks && let Some(window) = window {
            self.request_callback(window);
        }
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = render::source_bound(self.pane_bound(surface), scale, transform);
                let mut previous = self.windows.surface_cache.snapshots.remove(surface);
                let single_pixel = get_single_pixel_buffer(&buffer)
                    .ok()
                    .map(|pixel| Snapshot::single_pixel(pixel.rgba8888()));
                let dmabuf = get_dmabuf(&buffer).is_ok();
                let snapshot = single_pixel.or_else(|| {
                    if dmabuf {
                        readback::snapshot(surface)
                    } else {
                        with_buffer_contents(&buffer, |ptr, len, data| {
                            let viewport = with_states(surface, |states| {
                                *states.cached_state.get::<ViewportCachedState>().current()
                            });
                            let damage = buffer_damage(&damage, &data, scale, transform, viewport);
                            // SAFETY: Smithay keeps the mapped pool readable
                            // for this callback and supplies its pointer and
                            // length together.
                            unsafe {
                                Snapshot::copy_shm(ptr, len, &data, bound, &damage, &mut previous)
                            }
                        })
                        .ok()
                        .flatten()
                    }
                });
                buffer.release();
                let snapshot = match snapshot {
                    Some(snapshot) => {
                        if dmabuf && let Some(previous) = previous.take() {
                            self.windows
                                .surface_cache
                                .spares
                                .insert(surface.clone(), previous);
                        }
                        Some(snapshot)
                    }
                    None => previous,
                };
                if let Some(snapshot) = snapshot {
                    self.windows
                        .surface_cache
                        .snapshots
                        .insert(surface.clone(), snapshot);
                }
            }
            Some(BufferAssignment::Removed) => self.windows.surface_cache.forget(surface),
            None => {}
        }
        self.refresh_window_mapping(surface);
        if let Some(window) = window {
            if self.attachments.visible(window) && self.windows.catalog.drawable(window) {
                self.output.enter(surface);
            }
            let damage = self
                .windows
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
            self.attachments
                .for_each_showing_mut(window, |_, presentation| {
                    presentation.damage(surface, damage, changed);
                });
        }
    }

    /// A destroyed surface may not belong to any toplevel, so remove its
    /// snapshot here rather than relying on toplevel cleanup.
    fn destroyed(&mut self, surface: &WlSurface) {
        let window = self.window_for_surface(surface);
        self.windows.popups_dirty = true;
        self.windows.surface_cache.forget(surface);
        readback::destroyed(self, surface);
        if let Some(window) = window {
            if self.windows.id_by_surface.get(surface) == Some(&window) {
                self.unmap_window(window);
            } else {
                self.touch(window);
            }
        }
    }
}
