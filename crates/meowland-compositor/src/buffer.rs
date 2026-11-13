//! Surface commits, copied snapshots, and buffer protocol handling.

use std::collections::HashMap;

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::buffer_dimensions},
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    utils::{Buffer, Logical, Rectangle, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage,
            SurfaceAttributes, add_pre_commit_hook, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        shm::{ShmHandler, ShmState, with_buffer_contents},
        single_pixel_buffer::get_single_pixel_buffer,
        viewporter::{ViewportCachedState, ensure_viewport_valid},
    },
};

use super::{Compositor, Snapshot, readback, render, runtime::ClientState};

/// What the compositor retains per surface: the pixels the renderer reads,
/// and the bookkeeping that produces them. Every mutation goes through these
/// methods, so forgetting a surface clears both together.
#[derive(Default)]
pub(crate) struct SurfaceCache {
    snapshots: HashMap<WlSurface, Snapshot>,
    entries: HashMap<WlSurface, Entry>,
}

#[derive(Default)]
struct Entry {
    /// Pixel storage of a replaced DMA-BUF snapshot, reused by the next
    /// readback.
    spare: Option<Snapshot>,
    /// The last new buffer produced no snapshot, so the next one is copied
    /// whole.
    missed: bool,
    /// Size of the buffer the client last attached, which its viewport must
    /// fit even when the compositor kept older pixels.
    buffer_size: Option<Size<i32, Buffer>>,
}

impl SurfaceCache {
    /// Pixels of every surface with a usable buffer.
    pub(crate) const fn snapshots(&self) -> &HashMap<WlSurface, Snapshot> {
        &self.snapshots
    }

    pub(crate) fn forget(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
        self.entries.remove(surface);
    }

    /// Take the storage a readback for `surface` may fill.
    pub(crate) fn take_spare(&mut self, surface: &WlSurface) -> Option<Snapshot> {
        self.entries.get_mut(surface)?.spare.take()
    }

    /// Return unused readback storage, unless the surface's pixels were
    /// forgotten while the readback ran.
    pub(crate) fn return_spare(&mut self, surface: &WlSurface, spare: Snapshot) {
        if self.snapshots.contains_key(surface) {
            self.entries.entry(surface.clone()).or_default().spare = Some(spare);
        }
    }

    /// Start replacing a surface's pixels: whether the last new buffer was
    /// missed, and the current snapshot to patch or recycle.
    fn begin(&mut self, surface: &WlSurface) -> (bool, Option<Snapshot>) {
        let missed = self.entries.get(surface).is_some_and(|entry| entry.missed);
        (missed, self.snapshots.remove(surface))
    }

    /// Adopt a new snapshot, or keep `previous` and remember the miss when
    /// there is none. Returns whether this recovered from a miss, and the
    /// replaced snapshot.
    fn finish(
        &mut self,
        surface: &WlSurface,
        snapshot: Option<Snapshot>,
        previous: Option<Snapshot>,
    ) -> (bool, Option<Snapshot>) {
        let entry = self.entries.entry(surface.clone()).or_default();
        let was_missed = std::mem::replace(&mut entry.missed, snapshot.is_none());
        let recovered = snapshot.is_some() && was_missed;
        let (current, replaced) = if snapshot.is_some() {
            (snapshot, previous)
        } else {
            (previous, None)
        };
        if let Some(current) = current {
            self.snapshots.insert(surface.clone(), current);
        }
        (recovered, replaced)
    }

    /// Every surface with cached state.
    #[cfg(test)]
    pub(crate) fn surfaces(&self) -> impl Iterator<Item = &WlSurface> {
        self.snapshots.keys().chain(self.entries.keys())
    }

    fn valid_viewport(
        &mut self,
        surface: &WlSurface,
        assignment: Option<&BufferAssignment>,
        scale: i32,
        transform: Transform,
    ) -> bool {
        let size = match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let size = buffer_dimensions(buffer);
                self.entries.entry(surface.clone()).or_default().buffer_size = size;
                size
            }
            Some(BufferAssignment::Removed) => {
                if let Some(entry) = self.entries.get_mut(surface) {
                    entry.buffer_size = None;
                }
                None
            }
            None => self
                .entries
                .get(surface)
                .and_then(|entry| entry.buffer_size),
        };
        size.is_none_or(|size| {
            scale > 0
                && with_states(surface, |states| {
                    ensure_viewport_valid(states, size.to_logical(scale, transform))
                })
        })
    }
}

/// Translate committed surface damage into buffer coordinates for snapshot
/// patching.
///
/// Empty damage means the whole buffer may have changed. A viewport maps
/// surface damage through a source region, so patching falls back to a full
/// copy for those surfaces, as it does for a scale the protocol forbids.
fn buffer_damage(
    damage: &[Damage],
    size: Size<i32, Buffer>,
    scale: i32,
    transform: Transform,
    viewport: ViewportCachedState,
) -> Vec<Rectangle<i32, Buffer>> {
    if damage.is_empty() || scale < 1 {
        return Vec::new();
    }
    let surface = size.to_logical(scale, transform);
    let mut rects = Vec::with_capacity(damage.len());
    for damage in damage {
        let rect = match damage {
            Damage::Buffer(rect) => *rect,
            Damage::Surface(rect) => {
                if viewport.src.is_some() || viewport.dst.is_some() {
                    return Vec::new();
                }
                let Some(rect) = clip_surface_damage(*rect, surface) else {
                    continue;
                };
                rect.to_buffer(scale, transform, &surface)
            }
        };
        rects.push(rect);
    }
    rects
}

/// Clip client-controlled surface damage to the surface in wide arithmetic.
///
/// Smithay transforms and scales rectangles in `i32`, so unclipped extreme
/// coordinates would overflow before the snapshot copy clips them.
fn clip_surface_damage(
    rect: Rectangle<i32, Logical>,
    surface: Size<i32, Logical>,
) -> Option<Rectangle<i32, Logical>> {
    let clip = |start: i32, length: i32, extent: i32| {
        let start = i64::from(start);
        let low = start.max(0);
        let high = (start + i64::from(length)).min(i64::from(extent));
        (low < high).then(|| {
            (
                i32::try_from(low).expect("clipped start lies within the surface"),
                i32::try_from(high - low).expect("clipped length fits the surface"),
            )
        })
    };
    let (x, width) = clip(rect.loc.x, rect.size.w, surface.w)?;
    let (y, height) = clip(rect.loc.y, rect.size.h, surface.h)?;
    Some(Rectangle::new((x, y).into(), (width, height).into()))
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

    fn commit(&mut self, surface: &WlSurface) {
        self.windows.popups.commit(surface);
        let window = self.window_for_surface(surface);
        let (assignment, damage, scale, transform, callbacks) = committed_attributes(surface);
        if !self.windows.surface_cache.valid_viewport(
            surface,
            assignment.as_ref(),
            scale,
            transform,
        ) {
            return;
        }
        let new_buffer = matches!(assignment, Some(BufferAssignment::NewBuffer(_)));
        let mut recovered = false;
        self.update_remap_phase(surface, window, assignment.as_ref());
        if callbacks && let Some(window) = window {
            self.request_callback(window);
        }
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = render::source_bound(self.pane_bound(surface), scale, transform);
                let (missed, mut previous) = self.windows.surface_cache.begin(surface);
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
                            let size = Size::from((data.width, data.height));
                            let damage = if missed {
                                Vec::new()
                            } else {
                                buffer_damage(&damage, size, scale, transform, viewport)
                            };
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
                let cache = &mut self.windows.surface_cache;
                let replaced;
                (recovered, replaced) = cache.finish(surface, snapshot, previous);
                if dmabuf && let Some(replaced) = replaced {
                    cache.return_spare(surface, replaced);
                }
            }
            Some(BufferAssignment::Removed) => self.windows.surface_cache.forget(surface),
            None => {}
        }
        self.refresh_window_mapping(surface);
        if let Some(window) = window {
            if self.attachments.owner(window).is_some() && self.windows.catalog.drawable(window) {
                self.output.enter(surface);
            }
            let damage = self
                .windows
                .surface_cache
                .snapshots()
                .get(surface)
                .map_or_default(|snapshot| {
                    let geometry = render::surface_geometry(surface, snapshot);
                    if recovered || (new_buffer && damage.is_empty()) {
                        render::SurfaceDamage::full(&geometry)
                    } else {
                        render::SurfaceDamage::new(&damage, &geometry)
                    }
                });
            if let Some(pane) = self.attachments.owner(window)
                && let Some(state) = self.attachments.state_mut(pane)
            {
                state.damage(surface, damage);
            }
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

#[expect(
    clippy::significant_drop_tightening,
    reason = "`cached` borrows from the guard and must outlive `current`"
)]
fn committed_attributes(
    surface: &WlSurface,
) -> (Option<BufferAssignment>, Vec<Damage>, i32, Transform, bool) {
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        let current = cached.current();
        (
            current.buffer.take(),
            std::mem::take(&mut current.damage),
            current.buffer_scale,
            Transform::from(current.buffer_transform),
            !current.frame_callbacks.is_empty(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_only_commit_validates_against_the_retained_buffer() {
        use wayland_protocols::wp::viewporter::client::wp_viewport;

        let mut fixture = crate::fixture::Fixture::new();
        let top = fixture.toplevel("app");
        fixture.present(&top.surface, 4, 4, [0, 0, 0, 255]);
        let viewport = fixture.viewport(&top.surface);
        viewport.set_source(0.0, 0.0, 4.0, 4.0);
        top.surface.commit();
        fixture.roundtrip();
        viewport.set_source(0.0, 0.0, 5.0, 4.0);
        top.surface.commit();
        let error = fixture.protocol_error();
        assert_eq!(error.object_interface, "wp_viewport");
        assert_eq!(error.code, u32::from(wp_viewport::Error::OutOfBuffer));
    }

    #[test]
    fn failed_snapshot_then_partial_damage_recopies_the_entire_buffer() {
        use meowland_core::pixels::FrameSize;
        use meowland_protocol::{Follow, Selector, Target};
        use meowland_render::{Renderer, Scene};

        let mut fixture = crate::fixture::Fixture::new();
        let top = fixture.toplevel("app");
        fixture.present(&top.surface, 4, 4, [0, 0, 0, 255]);
        let window = fixture.window(&top);
        fixture.state.focus_pane(
            1.into(),
            Target::Selector(Selector::AppId("app".into())),
            Follow::First,
            FrameSize::new(64, 64).expect("pane size"),
        );
        let failed = fixture.buffer(256, 256, [0, 0, 255, 255]);
        top.surface.attach(Some(&failed), 0, 0);
        top.surface.damage_buffer(0, 0, 1, 1);
        top.surface.commit();
        fixture.roundtrip();
        let recovered = fixture.buffer(4, 4, [0, 0, 255, 255]);
        top.surface.attach(Some(&recovered), 0, 0);
        top.surface.damage_buffer(3, 3, 1, 1);
        top.surface.commit();
        fixture.roundtrip();

        let root = fixture.state.windows.by_id[&window].surface.wl_surface();
        let mut renderer = Renderer::default();
        let mut scene = Scene::default();
        renderer.update(
            &mut scene,
            Some(root),
            fixture.state.windows.surface_cache.snapshots(),
            FrameSize::new(4, 4).expect("frame size"),
            1.0,
            |_, _| {},
        );
        assert_eq!(&scene.pixels()[0..3], &[255, 0, 0]);
        assert_eq!(&scene.pixels()[45..48], &[255, 0, 0]);
    }

    /// Commit a DMA-BUF, then compose the surface into a 4x4 pane until it
    /// shows `expected` or the readback deadline passes.
    fn read_back(
        fixture: &mut crate::fixture::Fixture,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        buffer: &wayland_client::protocol::wl_buffer::WlBuffer,
        expected: &[u8],
    ) -> Vec<u8> {
        use meowland_core::pixels::FrameSize;
        use meowland_render::{Renderer, Scene};
        use smithay::reexports::wayland_server::Resource as _;
        use wayland_client::Proxy as _;

        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        let id = surface.id().protocol_id();
        let mut composed = Vec::new();
        for _ in 0..500 {
            fixture.roundtrip();
            let snapshots = &fixture.state.windows.surface_cache.snapshots();
            let root = snapshots
                .keys()
                .find(|surface| surface.id().protocol_id() == id);
            let mut scene = Scene::default();
            Renderer::default().update(
                &mut scene,
                root,
                snapshots,
                FrameSize::new(4, 4).expect("frame size"),
                1.0,
                |_, _| {},
            );
            composed = scene.pixels().to_vec();
            if composed == expected {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        composed
    }

    #[test]
    fn dmabuf_commits_are_read_back_through_the_gpu() {
        let mut fixture = crate::fixture::Fixture::new();
        let top = fixture.toplevel("app");
        let (Some(red), Some(green)) = (
            fixture.dmabuf(64, 4, [0, 0, 255, 255]),
            fixture.dmabuf(64, 4, [0, 255, 0, 0]),
        ) else {
            eprintln!("skipping: no GPU backend or /dev/udmabuf");
            return;
        };
        let red_rgb = [255, 0, 0].repeat(16);
        assert_eq!(
            read_back(&mut fixture, &top.surface, &red, &red_rgb),
            red_rgb
        );
        let green_rgb = [0, 255, 0].repeat(16);
        assert_eq!(
            read_back(&mut fixture, &top.surface, &green, &green_rgb),
            green_rgb,
            "a replacement buffer with an X channel of zero reads back opaque"
        );
    }

    const TRANSFORMS: [Transform; 8] = [
        Transform::Normal,
        Transform::_90,
        Transform::_180,
        Transform::_270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    fn surface_damage(x: i32, y: i32, width: i32, height: i32) -> Damage {
        Damage::Surface(Rectangle::new((x, y).into(), (width, height).into()))
    }

    #[test]
    fn extreme_surface_damage_is_clipped_inside_the_buffer() {
        let size = Size::<i32, Buffer>::from((12, 6));
        let damage = [
            surface_damage(i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            surface_damage(i32::MIN, 0, i32::MAX, 1),
            surface_damage(-1, -1, i32::MAX, i32::MAX),
            surface_damage(i32::MAX, i32::MAX, i32::MAX, i32::MAX),
            surface_damage(1, 1, 1, 1),
        ];
        let bounds = Rectangle::from_size(size);
        for scale in [1, 2, 3] {
            for transform in TRANSFORMS {
                let rects = buffer_damage(
                    &damage,
                    size,
                    scale,
                    transform,
                    ViewportCachedState::default(),
                );
                assert!(!rects.is_empty(), "{scale} {transform:?}");
                for rect in rects {
                    assert!(
                        rect.size.w > 0 && rect.size.h > 0 && bounds.contains_rect(rect),
                        "{rect:?} escapes {bounds:?} at scale {scale} {transform:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn surface_damage_overlapping_an_edge_keeps_its_visible_part() {
        let rects = buffer_damage(
            &[surface_damage(-5, 1, 7, i32::MAX)],
            Size::from((8, 6)),
            2,
            Transform::Normal,
            ViewportCachedState::default(),
        );
        assert_eq!(rects, [Rectangle::new((0, 2).into(), (4, 4).into())]);
    }

    #[test]
    fn damage_entirely_outside_the_surface_patches_nothing() {
        assert_eq!(
            clip_surface_damage(
                Rectangle::new((i32::MIN, 0).into(), (i32::MAX, 4).into()),
                Size::from((4, 4)),
            ),
            None
        );
    }
}
