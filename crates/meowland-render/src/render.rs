//! Surface traversal, hit testing, and frame composition.

use std::{collections::HashMap, ops::Range};

use meowland_core::pixels::FrameSize;
use smithay::{
    desktop::{PopupKind, PopupManager},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Physical, Point, Rectangle, Size},
    wayland::{
        compositor::{
            SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction, with_states,
            with_surface_tree_upward,
        },
        shell::xdg::SurfaceCachedState,
    },
};

use super::{
    Scene, Snapshot,
    damage::Layer,
    geometry::{SurfaceGeometry, rounded, surface_geometry},
};

/// Bound scratch storage independently of pane height.
const STRIP_ROWS: i32 = 16;

struct Target<'a> {
    pixels: &'a mut [u8],
    columns: Range<i32>,
    rows: Range<i32>,
}

/// Scratch allocations owned by the compositor, not by any client or pane.
#[derive(Default)]
pub struct Renderer {
    pixels: Vec<u8>,
    stack: SurfaceStack,
    layers: Vec<Layer>,
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
    /// Recompose damaged rectangles into a retained pane and return changed
    /// rows.
    ///
    /// Changes in surface order, placement, dimensions, viewport, scale, or
    /// transform invalidate the scene and force full composition. Otherwise
    /// only the bounding rectangle of accumulated content damage is sampled.
    /// The callback receives accumulated changed rows and the ready image
    /// prefix, allowing sequential publication while those pixels are hot.
    ///
    /// # Panics
    /// Panics if clipped coordinates cannot be represented as buffer offsets,
    /// or if the publication callback panics.
    pub fn update(
        &mut self,
        scene: &mut Scene,
        root: Option<&WlSurface>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        size: FrameSize,
        scale: f64,
        mut publish: impl FnMut(Option<&Range<usize>>, &[u8]),
    ) -> Option<Range<usize>> {
        scene.pixels.resize(size.rgb_len(), 0);
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let damage = self.prepare(scene, root, snapshots, size, scale)?;
        let covered = opaque_cover(snapshots, &scene.layers, size.width(), size.height(), scale);
        let layers = if covered {
            &scene.layers[scene.layers.len() - 1..]
        } else {
            &scene.layers
        };
        let columns = damage.loc.x..damage.loc.x + damage.size.w;
        let rows = damage.loc.y..damage.loc.y + damage.size.h;
        let row_bytes = usize::try_from(damage.size.w).expect("clipped width is positive") * 3;
        let mut changed: Option<Range<usize>> = None;
        for top in rows
            .clone()
            .step_by(usize::try_from(STRIP_ROWS).expect("strip rows fit usize"))
        {
            let bottom = (top + STRIP_ROWS).min(rows.end);
            self.pixels.resize(
                row_bytes * usize::try_from(bottom - top).expect("strip height is positive"),
                0,
            );
            if !covered {
                self.pixels.fill(0);
            }
            let mut target = Target {
                pixels: &mut self.pixels,
                columns: columns.clone(),
                rows: top..bottom,
            };
            for layer in layers {
                if let Some(snapshot) = snapshots.get(&layer.surface) {
                    blit(snapshot, &layer.geometry, layer.origin, &mut target, scale);
                }
            }
            for (offset, pixels) in self.pixels.chunks_exact(row_bytes).enumerate() {
                let row = usize::try_from(top).expect("clipped row is nonnegative") + offset;
                let start = row * size.rgb_stride()
                    + usize::try_from(columns.start).expect("clipped column is nonnegative") * 3;
                let retained = &mut scene.pixels[start..start + row_bytes];
                if retained != pixels {
                    retained.copy_from_slice(pixels);
                    changed = Some(changed.map_or(row..row + 1, |changed| changed.start..row + 1));
                }
            }
            let ready =
                usize::try_from(bottom).expect("clipped row is nonnegative") * size.rgb_stride();
            publish(changed.as_ref(), &scene.pixels[..ready]);
        }
        changed
    }

    fn prepare(
        &mut self,
        scene: &mut Scene,
        root: Option<&WlSurface>,
        snapshots: &HashMap<WlSurface, Snapshot>,
        size: FrameSize,
        scale: f64,
    ) -> Option<Rectangle<i32, Physical>> {
        if let Some(root) = root {
            self.stack.rebuild(root);
        } else {
            self.stack.clear();
        }
        self.layers.clear();
        self.layers
            .extend(self.stack.surfaces.iter().filter_map(|(surface, origin)| {
                let snapshot = snapshots.get(surface)?;
                Some(Layer {
                    surface: surface.clone(),
                    origin: *origin,
                    geometry: surface_geometry(surface, snapshot),
                })
            }));
        self.stack.clear();
        let bounds: Rectangle<i32, Physical> = Rectangle::from_size(
            (
                i32::try_from(size.width()).expect("validated pane width fits i32"),
                i32::try_from(size.height()).expect("validated pane height fits i32"),
            )
                .into(),
        );
        let damage = if scene.basis != Some((size, scale)) || scene.layers != self.layers {
            Some(bounds)
        } else {
            self.layers
                .iter()
                .filter_map(|layer| {
                    let damage = scene.damaged(layer)?;
                    let dst = placement(layer.origin, layer.geometry.dst, scale)?;
                    let rect = Rectangle::new(
                        (
                            f64::from(dst.left)
                                + damage.loc.x * f64::from(dst.width)
                                    / f64::from(layer.geometry.dst.w),
                            f64::from(dst.top)
                                + damage.loc.y * f64::from(dst.height)
                                    / f64::from(layer.geometry.dst.h),
                        )
                            .into(),
                        (
                            damage.size.w * f64::from(dst.width) / f64::from(layer.geometry.dst.w),
                            damage.size.h * f64::from(dst.height) / f64::from(layer.geometry.dst.h),
                        )
                            .into(),
                    );
                    rect.to_i32_up::<i32>().intersection(bounds)
                })
                .reduce(Rectangle::merge)
        };
        std::mem::swap(&mut scene.layers, &mut self.layers);
        scene.basis = Some((size, scale));
        scene.damage.clear();
        self.layers.clear();
        damage
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
fn popup_origin(
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
    snapshot: &Snapshot,
    geometry: &SurfaceGeometry,
    origin: Point<i32, Logical>,
    target: &mut Target<'_>,
    scale: f64,
) {
    let Some(dst) = placement(origin, geometry.dst, scale) else {
        return;
    };
    let clip_left = dst.left.max(target.columns.start);
    let clip_top = dst.top.max(target.rows.start);
    let clip_right = dst.left.saturating_add(dst.width).min(target.columns.end);
    let clip_bottom = dst.top.saturating_add(dst.height).min(target.rows.end);
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
        blit_scaled::<true>(snapshot, geometry, dst, clip, target);
    } else {
        blit_scaled::<false>(snapshot, geometry, dst, clip, target);
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
    let pane_stride = usize::try_from(target.columns.end - target.columns.start)
        .expect("clipped width is positive");
    let clip_x = usize::try_from(clip.loc.x - target.columns.start)
        .expect("clipped column lies inside the target");
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
            let row =
                usize::try_from(y - target.rows.start).expect("clipped row lies inside the target");
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
    let pane_stride = usize::try_from(target.columns.end - target.columns.start)
        .expect("clipped width is positive")
        * 3;
    let first = usize::try_from(clip.loc.x - left).expect("clipped source x is nonnegative");
    let first_row = usize::try_from(clip.loc.y - top).expect("clipped source y is nonnegative");
    let clip_x = usize::try_from(clip.loc.x - target.columns.start)
        .expect("clipped column lies inside the target");
    let clip_y = usize::try_from(clip.loc.y - target.rows.start)
        .expect("clipped row lies inside the target");
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
    layers: &[Layer],
    width: u32,
    height: u32,
    scale: f64,
) -> bool {
    let Some(layer) = layers.last() else {
        return false;
    };
    let Some(snapshot) = snapshots.get(&layer.surface) else {
        return false;
    };
    let Some(dst) = placement(layer.origin, layer.geometry.dst, scale) else {
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
