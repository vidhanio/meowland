//! Surface traversal, hit testing, and frame composition.

use std::collections::HashMap;

use smithay::{
    utils::Point,
    wayland::compositor::{SurfaceData, TraversalAction, with_surface_tree_upward},
};

use super::{
    Logical, PopupKind, PopupManager, Rectangle, Size, Snapshot, State, SubsurfaceCachedState,
    SurfaceAttributes, SurfaceCachedState, ViewportCachedState, Window, WlSurface, with_states,
};
use crate::pixels::FrameSize;

/// Scratch allocations owned by the compositor, not by any client or pane.
#[derive(Default)]
pub(super) struct Renderer {
    pixels: Vec<u8>,
    pub(super) stack: SurfaceStack,
}

impl Renderer {
    /// Compose one pane while borrowing scene state only for this draw.
    pub(super) fn render(
        &mut self,
        windows: &HashMap<u64, Window>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        window: u64,
        size: FrameSize,
    ) -> &[u8] {
        if let Some(window) = windows.get(&window) {
            self.stack.rebuild(window.surface.wl_surface());
        } else {
            self.stack.clear();
        }
        let (width, height) = (size.width(), size.height());
        self.pixels.resize(size.rgb_len(), 0);
        let first = if opaque_cover(snapshots, &self.stack.surfaces, width, height) {
            // The topmost opaque surface hides the backdrop and everything
            // below.
            self.stack.surfaces.len() - 1
        } else {
            self.pixels.fill(0);
            0
        };
        for (surface, origin) in &self.stack.surfaces[first..] {
            blit(snapshots, surface, *origin, &mut self.pixels, width, height);
        }
        // Keep allocations, but never keep clients alive through scratch
        // handles.
        self.stack.clear();
        &self.pixels
    }
}

/// Window surfaces in bottom-to-top order, with origins in pane coordinates.
#[derive(Default)]
pub(super) struct SurfaceStack {
    pub(super) surfaces: Vec<(WlSurface, Point<i32, Logical>)>,
    popups: Vec<(PopupKind, Point<i32, Logical>)>,
}

impl SurfaceStack {
    pub(super) fn rebuild(&mut self, root: &WlSurface) {
        self.clear();
        collect_surface(root, (0, 0).into(), &mut self.surfaces);
        let geometry = window_geometry(root);
        self.popups.extend(PopupManager::popups_for_surface(root));
        // Smithay lists popup children before parents; drawing needs the
        // reverse.
        for (popup, location) in self.popups.drain(..).rev() {
            let origin = popup_origin(geometry, location, &popup);
            collect_surface(popup.wl_surface(), origin, &mut self.surfaces);
        }
    }

    pub(super) fn clear(&mut self) {
        self.surfaces.clear();
        self.popups.clear();
    }
}

pub(super) fn surface_stack(root: &WlSurface) -> Vec<(WlSurface, Point<i32, Logical>)> {
    let mut stack = SurfaceStack::default();
    stack.rebuild(root);
    stack.surfaces
}

/// Popup positions are measured against the parent's window geometry origin.
pub(super) fn window_geometry(surface: &WlSurface) -> Point<i32, Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map_or_else(|| (0, 0).into(), |geometry| geometry.loc)
    })
}

/// Convert a popup's window-geometry position to its surface origin in the
/// pane.
pub(super) fn popup_origin(
    parent_geometry: Point<i32, Logical>,
    location: Point<i32, Logical>,
    popup: &PopupKind,
) -> Point<i32, Logical> {
    let geometry = popup.geometry().loc;
    let coordinate = |parent, location, geometry| {
        (i64::from(parent) + i64::from(location) - i64::from(geometry))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    Point::from((
        coordinate(parent_geometry.x, location.x, geometry.x),
        coordinate(parent_geometry.y, location.y, geometry.y),
    ))
}

pub(super) fn collect_surface(
    surface: &WlSurface,
    origin: Point<i32, Logical>,
    stack: &mut Vec<(WlSurface, Point<i32, Logical>)>,
) {
    let position = |node: &WlSurface, states: &SurfaceData, parent: &(i64, i64)| {
        if node == surface {
            return *parent;
        }
        let offset = states
            .cached_state
            .get::<SubsurfaceCachedState>()
            .current()
            .location;
        (
            parent.0 + i64::from(offset.x),
            parent.1 + i64::from(offset.y),
        )
    };
    // Smithay preserves client stacking, including children below their parent.
    // Wide coordinates allow offscreen descendants to return into view.
    with_surface_tree_upward(
        surface,
        (i64::from(origin.x), i64::from(origin.y)),
        |node, states, parent| TraversalAction::DoChildren(position(node, states, parent)),
        |node, states, parent| {
            let (x, y) = position(node, states, parent);
            stack.push((
                node.clone(),
                (
                    x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                    y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                )
                    .into(),
            ));
        },
        |_, _, _| true,
    );
}

/// Blend premultiplied surface pixels into the pane, applying its viewport.
fn blit(
    snapshots: &HashMap<WlSurface, Snapshot>,
    surface: &WlSurface,
    origin: Point<i32, Logical>,
    out: &mut [u8],
    width: u32,
    height: u32,
) {
    let Some(snapshot) = snapshots.get(surface) else {
        return;
    };
    let viewport = viewport_of(surface);
    let unscaled = viewport.src.is_none() && viewport.dst.is_none();
    let (src, dst) = extents(snapshot, viewport);
    if dst.w <= 0 || dst.h <= 0 {
        return;
    }
    let left = origin.x;
    let top = origin.y;
    let clip_left = left.max(0);
    let clip_top = top.max(0);
    // Client-chosen extents can overflow if added without saturation.
    let clip_right = left.saturating_add(dst.w).min(width as i32);
    let clip_bottom = top.saturating_add(dst.h).min(height as i32);
    if clip_right <= clip_left || clip_bottom <= clip_top {
        return;
    }
    let clip = Rectangle::new(
        (clip_left, clip_top).into(),
        (clip_right - clip_left, clip_bottom - clip_top).into(),
    );
    if unscaled {
        blit_unscaled(snapshot, left, top, clip, out, width);
        return;
    }
    if snapshot.opaque {
        blit_scaled::<true>(snapshot, origin, src, dst, clip, out, width);
    } else {
        blit_scaled::<false>(snapshot, origin, src, dst, clip, out, width);
    }
}

fn blit_scaled<const OPAQUE: bool>(
    snapshot: &Snapshot,
    origin: Point<i32, Logical>,
    src: Rectangle<f64, Logical>,
    dst: Size<i32, Logical>,
    clip: Rectangle<i32, Logical>,
    out: &mut [u8],
    width: u32,
) {
    let last_x = snapshot.width as usize - 1;
    let last_y = snapshot.height as usize - 1;
    let stride = snapshot.width as usize;
    let pixels = snapshot.pixels.as_chunks::<4>().0;
    let columns = clip.size.w as usize;
    // Reuse each column map across rows without allocating a frame-width map.
    let mut source_columns = [0; 256];
    for start in (0..columns).step_by(source_columns.len()) {
        let count = (columns - start).min(source_columns.len());
        let source_columns = &mut source_columns[..count];
        for (column, source_column) in source_columns.iter_mut().enumerate() {
            let x = clip.loc.x + (start + column) as i32;
            let sample =
                src.loc.x + (f64::from(x - origin.x) + 0.5) * src.size.w / f64::from(dst.w);
            *source_column = (sample.floor().max(0.0) as usize).min(last_x);
        }
        for y in clip.loc.y..clip.loc.y + clip.size.h {
            let sample =
                src.loc.y + (f64::from(y - origin.y) + 0.5) * src.size.h / f64::from(dst.h);
            let row = (sample.floor().max(0.0) as usize).min(last_y) * stride;
            let to = (y as usize * width as usize + clip.loc.x as usize + start) * 3;
            let rgb_row = out[to..to + count * 3].as_chunks_mut::<3>().0;
            for (&column, rgb) in source_columns.iter().zip(rgb_row) {
                let pixel = pixels[row + column];
                if OPAQUE {
                    rgb.copy_from_slice(&pixel[..3]);
                } else {
                    blend_pixel(pixel, rgb);
                }
            }
        }
    }
}

/// Unscaled pixels map directly to snapshot offsets from the surface origin.
pub(super) fn blit_unscaled(
    snapshot: &Snapshot,
    left: i32,
    top: i32,
    clip: Rectangle<i32, Logical>,
    out: &mut [u8],
    width: u32,
) {
    if snapshot.opaque {
        blit_unscaled_inner::<true>(snapshot, left, top, clip, out, width);
    } else {
        blit_unscaled_inner::<false>(snapshot, left, top, clip, out, width);
    }
}

fn blit_unscaled_inner<const OPAQUE: bool>(
    snapshot: &Snapshot,
    left: i32,
    top: i32,
    clip: Rectangle<i32, Logical>,
    out: &mut [u8],
    width: u32,
) {
    let columns = clip.size.w as usize;
    let row_bytes = columns * 4;
    let stride = snapshot.width as usize * 4;
    let pane_stride = width as usize * 3;
    // Clipping to snapshot bounds keeps the indexed spans below in bounds.
    let first = (clip.loc.x - left) as usize;
    let first_row = (clip.loc.y - top) as usize;
    for row in 0..clip.size.h as usize {
        let from = (first_row + row) * stride + first * 4;
        let source = snapshot.pixels[from..from + row_bytes].as_chunks::<4>().0;
        let at = (clip.loc.y as usize + row) * pane_stride + clip.loc.x as usize * 3;
        let dest = out[at..at + columns * 3].as_chunks_mut::<3>().0;
        for (pixel, rgb) in source.iter().zip(dest) {
            if OPAQUE {
                rgb.copy_from_slice(&pixel[..3]);
            } else {
                blend_pixel(*pixel, rgb);
            }
        }
    }
}

/// The snapshot is premultiplied; preserve the same rounding in both the
/// direct-copy and viewport-sampling paths.
#[inline]
fn blend_pixel(pixel: [u8; 4], rgb: &mut [u8; 3]) {
    let alpha = u32::from(pixel[3]);
    match alpha {
        0 => {}
        255 => rgb.copy_from_slice(&pixel[..3]),
        _ => {
            let inverse = 255 - alpha;
            for channel in 0..3 {
                let under = u32::from(rgb[channel]);
                rgb[channel] = (u32::from(pixel[channel]) + (under * inverse + 127) / 255) as u8;
            }
        }
    }
}

fn opaque_cover(
    snapshots: &HashMap<WlSurface, Snapshot>,
    stack: &[(WlSurface, Point<i32, Logical>)],
    width: u32,
    height: u32,
) -> bool {
    let Some((surface, origin)) = stack.last() else {
        return false;
    };
    let Some(snapshot) = snapshots.get(surface) else {
        return false;
    };
    let viewport = viewport_of(surface);
    snapshot.opaque
        && viewport.src.is_none()
        && viewport.dst.is_none()
        && origin.x <= 0
        && origin.y <= 0
        && origin.x.saturating_add(snapshot.width as i32) >= width as i32
        && origin.y.saturating_add(snapshot.height as i32) >= height as i32
}

pub(super) fn extents(
    snapshot: &Snapshot,
    viewport: ViewportCachedState,
) -> (Rectangle<f64, Logical>, Size<i32, Logical>) {
    let full = Rectangle::new(
        (0.0, 0.0).into(),
        (f64::from(snapshot.width), f64::from(snapshot.height)).into(),
    );
    let src = viewport.src.unwrap_or(full);
    let dst = viewport
        .dst
        .unwrap_or_else(|| Size::from((src.size.w.round() as i32, src.size.h.round() as i32)));
    (src, dst)
}

pub(super) fn viewport_of(surface: &WlSurface) -> ViewportCachedState {
    with_states(surface, |states| {
        *states.cached_state.get::<ViewportCachedState>().current()
    })
}

impl State {
    /// Smithay expects the surface origin, not the local point, for pointer
    /// focus.
    pub(super) fn hit_test(
        &mut self,
        window: u64,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let root = self.windows.get(&window)?.surface.wl_surface();
        self.renderer.stack.rebuild(root);
        let hit = self
            .renderer
            .stack
            .surfaces
            .iter()
            .rev()
            .find_map(|(surface, origin)| {
                let snapshot = self.snapshots.get(surface)?;
                let (_, dst) = extents(snapshot, viewport_of(surface));
                let local =
                    Point::<f64, Logical>::from((x - f64::from(origin.x), y - f64::from(origin.y)));
                if local.x < 0.0
                    || local.y < 0.0
                    || local.x >= f64::from(dst.w)
                    || local.y >= f64::from(dst.h)
                {
                    return None;
                }
                let inside = with_states(surface, |states| {
                    states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .input_region
                        .as_ref()
                        .is_none_or(|region| {
                            region.contains((local.x.floor() as i32, local.y.floor() as i32))
                        })
                });
                inside.then(|| {
                    (
                        surface.clone(),
                        Point::from((f64::from(origin.x), f64::from(origin.y))),
                    )
                })
            });
        self.renderer.stack.clear();
        hit
    }
}
