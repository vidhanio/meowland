//! Surface traversal, hit testing, and frame composition.

use std::collections::HashMap;
use std::ops::Range;

use meowland_core::pixels::FrameSize;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{
    SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction, with_states,
    with_surface_tree_upward,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;

use super::Snapshot;
use super::geometry::{SurfaceGeometry, rounded, surface_geometry};

/// Bound composition scratch space and batch sequential shared-object writes.
const STRIP_ROWS: u32 = 16;

struct Target<'a> {
    pixels: &'a mut [u8],
    width: u32,
    rows: Range<u32>,
}

/// Scratch allocations owned by the compositor, not by any client or pane.
#[derive(Default)]
pub struct Renderer {
    pixels: Vec<u8>,
    stack: SurfaceStack,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("bytes", &self.pixels.len())
            .field("surfaces", &self.stack.surfaces.len())
            .finish_non_exhaustive()
    }
}

impl Renderer {
    /// Compose one pane at unit UI scale while borrowing scene state only for
    /// this draw.
    pub fn render(
        &mut self,
        root: Option<&WlSurface>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        size: FrameSize,
    ) -> &[u8] {
        self.render_scaled(root, snapshots, size, 1.0)
    }

    /// Compose logical surface geometry into a physical pane at `scale`.
    pub fn render_scaled(
        &mut self,
        root: Option<&WlSurface>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        size: FrameSize,
        scale: f64,
    ) -> &[u8] {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        if let Some(root) = root {
            self.stack.rebuild(root);
        } else {
            self.stack.clear();
        }
        let (width, height) = (size.width(), size.height());
        self.pixels.resize(size.rgb_len(), 0);
        let first = if opaque_cover(snapshots, &self.stack.surfaces, width, height, scale) {
            self.stack.surfaces.len() - 1
        } else {
            self.pixels.fill(0);
            0
        };
        for (surface, origin) in &self.stack.surfaces[first..] {
            blit(
                snapshots,
                surface,
                *origin,
                &mut Target {
                    pixels: &mut self.pixels,
                    width,
                    rows: 0..height,
                },
                scale,
            );
        }
        self.stack.clear();
        &self.pixels
    }

    /// Compose bounded RGB strips directly into a caller-owned publication sink.
    ///
    /// The callback receives consecutive full-width rows in top-to-bottom order.
    /// Scratch storage is reused and never grows to a full frame for this path.
    ///
    /// # Panics
    /// Propagates panics from the publication callback.
    pub fn render_rows(
        &mut self,
        root: Option<&WlSurface>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        size: FrameSize,
        scale: f64,
        mut emit: impl FnMut(u32, &[u8]),
    ) {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        if let Some(root) = root {
            self.stack.rebuild(root);
        } else {
            self.stack.clear();
        }
        let (width, height) = (size.width(), size.height());
        let covered = opaque_cover(snapshots, &self.stack.surfaces, width, height, scale);
        let first = if covered {
            self.stack.surfaces.len() - 1
        } else {
            0
        };
        for top in (0..height).step_by(usize::try_from(STRIP_ROWS).expect("strip rows fit usize")) {
            let bottom = (top + STRIP_ROWS).min(height);
            self.pixels.resize(
                size.rgb_stride() * usize::try_from(bottom - top).expect("strip height fits usize"),
                0,
            );
            if !covered {
                self.pixels.fill(0);
            }
            let mut target = Target {
                pixels: &mut self.pixels,
                width,
                rows: top..bottom,
            };
            for (surface, origin) in &self.stack.surfaces[first..] {
                blit(snapshots, surface, *origin, &mut target, scale);
            }
            emit(top, &self.pixels);
        }
        self.stack.clear();
    }
}

/// Window surfaces in bottom-to-top order, with origins in pane coordinates.
#[derive(Default)]
struct SurfaceStack {
    surfaces: Vec<(WlSurface, Point<i32, Logical>)>,
    popups: Vec<(PopupKind, Point<i32, Logical>)>,
}

impl SurfaceStack {
    fn rebuild(&mut self, root: &WlSurface) {
        self.clear();
        collect_surface(root, (0, 0).into(), &mut self.surfaces);
        let geometry = window_geometry(root);
        self.popups.extend(PopupManager::popups_for_surface(root));
        for (popup, location) in self.popups.drain(..).rev() {
            let origin = popup_origin(geometry, location, &popup);
            collect_surface(popup.wl_surface(), origin, &mut self.surfaces);
        }
    }

    fn clear(&mut self) {
        self.surfaces.clear();
        self.popups.clear();
    }
}

/// Popup positions are measured against the parent's window geometry origin.
#[must_use]
pub fn window_geometry(surface: &WlSurface) -> Point<i32, Logical> {
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
#[must_use]
pub fn popup_origin(
    parent_geometry: Point<i32, Logical>,
    location: Point<i32, Logical>,
    popup: &PopupKind,
) -> Point<i32, Logical> {
    let geometry = popup.geometry().loc;
    let coordinate = |parent, location, geometry| {
        coordinate(i64::from(parent) + i64::from(location) - i64::from(geometry))
    };
    Point::from((
        coordinate(parent_geometry.x, location.x, geometry.x),
        coordinate(parent_geometry.y, location.y, geometry.y),
    ))
}

fn collect_surface(
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
    with_surface_tree_upward(
        surface,
        (i64::from(origin.x), i64::from(origin.y)),
        |node, states, parent| TraversalAction::DoChildren(position(node, states, parent)),
        |node, states, parent| {
            let (x, y) = position(node, states, parent);
            stack.push((node.clone(), (coordinate(x), coordinate(y)).into()));
        },
        |_, _, _| true,
    );
}

/// Blend premultiplied surface pixels into the pane, applying its viewport.
fn blit(
    snapshots: &HashMap<WlSurface, Snapshot>,
    surface: &WlSurface,
    origin: Point<i32, Logical>,
    target: &mut Target<'_>,
    scale: f64,
) {
    let Some(snapshot) = snapshots.get(surface) else {
        return;
    };
    let geometry = surface_geometry(surface, snapshot);
    let Some(dst) = placement(origin, geometry.dst, scale) else {
        return;
    };
    let clip_left = dst.left.max(0);
    let clip_top = dst.top.max(coordinate(i64::from(target.rows.start)));
    let clip_right = dst
        .left
        .saturating_add(dst.width)
        .min(coordinate(i64::from(target.width)));
    let clip_bottom = dst
        .top
        .saturating_add(dst.height)
        .min(coordinate(i64::from(target.rows.end)));
    if clip_right <= clip_left || clip_bottom <= clip_top {
        return;
    }
    let clip = Rectangle::new(
        (clip_left, clip_top).into(),
        (clip_right - clip_left, clip_bottom - clip_top).into(),
    );
    if geometry.direct() && (scale - 1.0).abs() <= f64::EPSILON {
        blit_unscaled(snapshot, dst.left, dst.top, clip, target);
        return;
    }
    if snapshot.opaque {
        blit_scaled::<true>(snapshot, &geometry, dst, clip, target);
    } else {
        blit_scaled::<false>(snapshot, &geometry, dst, clip, target);
    }
}

#[derive(Clone, Copy)]
struct Placement {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

fn placement(
    origin: Point<i32, Logical>,
    dst: Size<i32, Logical>,
    scale: f64,
) -> Option<Placement> {
    if dst.w <= 0 || dst.h <= 0 {
        return None;
    }
    let left = scaled_edge(i64::from(origin.x), scale);
    let top = scaled_edge(i64::from(origin.y), scale);
    let right = scaled_edge(i64::from(origin.x) + i64::from(dst.w), scale);
    let bottom = scaled_edge(i64::from(origin.y) + i64::from(dst.h), scale);
    let width = right.saturating_sub(left);
    let height = bottom.saturating_sub(top);
    (width > 0 && height > 0).then_some(Placement {
        left,
        top,
        width,
        height,
    })
}

fn coordinate(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "edges are sums of two i32 coordinates, exactly representable in f64"
)]
fn scaled_edge(value: i64, scale: f64) -> i32 {
    rounded(value as f64 * scale)
}

fn blit_scaled<const OPAQUE: bool>(
    snapshot: &Snapshot,
    geometry: &SurfaceGeometry,
    dst: Placement,
    clip: Rectangle<i32, Logical>,
    target: &mut Target<'_>,
) {
    let stride = usize::try_from(snapshot.width).expect("validated snapshot width fits usize");
    let last_x = stride - 1;
    let last_y =
        usize::try_from(snapshot.height).expect("validated snapshot height fits usize") - 1;
    let pixels = snapshot.pixels.as_chunks::<4>().0;
    let columns = usize::try_from(clip.size.w).expect("clipped width is positive");
    let pane_stride = usize::try_from(target.width).expect("validated pane width fits usize");
    let clip_x = usize::try_from(clip.loc.x).expect("clipping removed negative x coordinates");
    let swapped = geometry.swapped();
    let mut source_columns = [0; 256];
    for start in (0..columns).step_by(source_columns.len()) {
        let count = (columns - start).min(source_columns.len());
        let source_columns = &mut source_columns[..count];
        for (column, source_column) in source_columns.iter_mut().enumerate() {
            let x = clip.loc.x
                + i32::try_from(start + column).expect("column lies within the clipped pane");
            let sample = geometry.src.loc.x
                + (f64::from(x) + 0.5 - f64::from(dst.left)) * geometry.src.size.w
                    / f64::from(dst.width);
            *source_column = if swapped {
                pixel_index(geometry.raw_y_from_x(sample), last_y)
            } else {
                pixel_index(geometry.raw_x(sample), last_x)
            };
        }
        for y in clip.loc.y..clip.loc.y + clip.size.h {
            let sample = geometry.src.loc.y
                + (f64::from(y) + 0.5 - f64::from(dst.top)) * geometry.src.size.h
                    / f64::from(dst.height);
            let (base, advance) = if swapped {
                (pixel_index(geometry.raw_x_from_y(sample), last_x), stride)
            } else {
                (pixel_index(geometry.raw_y(sample), last_y) * stride, 1)
            };
            let row = usize::try_from(y).expect("clipping removed negative y coordinates")
                - usize::try_from(target.rows.start).expect("validated strip offset fits usize");
            let to = (row * pane_stride + clip_x + start) * 3;
            let rgb_row = target.pixels[to..to + count * 3].as_chunks_mut::<3>().0;
            for (&column, rgb) in source_columns.iter().zip(rgb_row) {
                let pixel = pixels[base + column * advance];
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
fn blit_unscaled(
    snapshot: &Snapshot,
    left: i32,
    top: i32,
    clip: Rectangle<i32, Logical>,
    target: &mut Target<'_>,
) {
    if snapshot.opaque {
        blit_unscaled_inner::<true>(snapshot, left, top, clip, target);
    } else {
        blit_unscaled_inner::<false>(snapshot, left, top, clip, target);
    }
}

fn blit_unscaled_inner<const OPAQUE: bool>(
    snapshot: &Snapshot,
    left: i32,
    top: i32,
    clip: Rectangle<i32, Logical>,
    target: &mut Target<'_>,
) {
    let columns = usize::try_from(clip.size.w).expect("clipped width is positive");
    let row_bytes = columns * 4;
    let stride = usize::try_from(snapshot.width).expect("validated snapshot width fits usize") * 4;
    let pane_stride = usize::try_from(target.width).expect("validated pane width fits usize") * 3;
    let first = usize::try_from(clip.loc.x - left).expect("clipped source x is nonnegative");
    let first_row = usize::try_from(clip.loc.y - top).expect("clipped source y is nonnegative");
    let clip_x = usize::try_from(clip.loc.x).expect("clipped destination x is nonnegative");
    let clip_y = usize::try_from(clip.loc.y).expect("clipped destination y is nonnegative")
        - usize::try_from(target.rows.start).expect("validated strip offset fits usize");
    for row in 0..usize::try_from(clip.size.h).expect("clipped height is positive") {
        let from = (first_row + row) * stride + first * 4;
        let source = snapshot.pixels[from..from + row_bytes].as_chunks::<4>().0;
        let at = (clip_y + row) * pane_stride + clip_x * 3;
        let dest = target.pixels[at..at + columns * 3].as_chunks_mut::<3>().0;
        for (pixel, rgb) in source.iter().zip(dest) {
            if OPAQUE {
                rgb.copy_from_slice(&pixel[..3]);
            } else {
                blend_pixel(*pixel, rgb);
            }
        }
    }
}

#[inline]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "sampling floors nonnegative coordinates and clamps them to the last valid pixel"
)]
fn pixel_index(value: f64, last: usize) -> usize {
    (value.floor().max(0.0) as usize).min(last)
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
                let blended = u32::from(pixel[channel]) + (under * inverse + 127) / 255;
                rgb[channel] = u8::try_from(blended).unwrap_or(u8::MAX);
            }
        }
    }
}

fn opaque_cover(
    snapshots: &HashMap<WlSurface, Snapshot>,
    stack: &[(WlSurface, Point<i32, Logical>)],
    width: u32,
    height: u32,
    scale: f64,
) -> bool {
    let Some((surface, origin)) = stack.last() else {
        return false;
    };
    let Some(snapshot) = snapshots.get(surface) else {
        return false;
    };
    let geometry = surface_geometry(surface, snapshot);
    let Some(dst) = placement(*origin, geometry.dst, scale) else {
        return false;
    };
    snapshot.opaque
        && dst.left <= 0
        && dst.top <= 0
        && dst.left.saturating_add(dst.width) >= coordinate(i64::from(width))
        && dst.top.saturating_add(dst.height) >= coordinate(i64::from(height))
}

impl Renderer {
    /// Return the hit surface and its origin, as expected by Smithay's pointer
    /// focus.
    pub fn hit_test(
        &mut self,
        root: &WlSurface,
        snapshots: &HashMap<WlSurface, Snapshot>,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.stack.rebuild(root);
        let hit = self
            .stack
            .surfaces
            .iter()
            .rev()
            .find_map(|(surface, origin)| {
                let snapshot = snapshots.get(surface)?;
                let dst = surface_geometry(surface, snapshot).dst;
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
                            region.contains((rounded(local.x.floor()), rounded(local.y.floor())))
                        })
                });
                inside.then(|| {
                    (
                        surface.clone(),
                        Point::from((f64::from(origin.x), f64::from(origin.y))),
                    )
                })
            });
        self.stack.clear();
        hit
    }
}
