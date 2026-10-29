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

/// Changed pixel rectangle of a recomposed pane frame.
///
/// Columns are the recomposed damage range rather than a per-pixel comparison:
/// the renderer already samples every pixel in that range, and narrowing it
/// would need a second pass over the composed rows.
#[derive(Clone, Debug)]
pub struct Changed {
    /// Recomposed columns, left-inclusive.
    pub columns: Range<u32>,
    /// Rows whose pixels changed, top-inclusive.
    pub rows: Range<u32>,
}

impl Renderer {
    /// Recompose damaged rectangles into a retained pane and return the changed
    /// rectangle.
    ///
    /// Changes in surface order, placement, dimensions, viewport, scale, or
    /// transform invalidate the scene and force full composition. Otherwise
    /// only the bounding rectangle of accumulated content damage is sampled.
    /// The callback receives the accumulated changed rectangle and the ready
    /// image prefix, allowing sequential publication while those pixels are
    /// hot.
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
        mut publish: impl FnMut(Option<&Changed>, &[u8]),
    ) -> Option<Changed> {
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
        let changed_columns = u32::try_from(columns.start).expect("clipped column is nonnegative")
            ..u32::try_from(columns.end).expect("clipped column is nonnegative");
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
            let rect = changed.as_ref().map(|changed| Changed {
                columns: changed_columns.clone(),
                rows: u32::try_from(changed.start).expect("clipped row is nonnegative")
                    ..u32::try_from(changed.end).expect("clipped row is nonnegative"),
            });
            publish(rect.as_ref(), &scene.pixels[..ready]);
        }
        changed.map(|rows| Changed {
            columns: changed_columns,
            rows: u32::try_from(rows.start).expect("clipped row is nonnegative")
                ..u32::try_from(rows.end).expect("clipped row is nonnegative"),
        })
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
    if snapshot.opaque() {
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
    if snapshot.opaque() {
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
    snapshot.opaque()
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use smithay::{
        utils::{Logical, Point, Rectangle, Size, Transform},
        wayland::viewporter::ViewportCachedState,
    };

    use super::*;
    use crate::{geometry::SurfaceGeometry, snapshot::Opacity};

    fn snapshot(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Snapshot {
        let len = usize::try_from(width)
            .unwrap()
            .checked_mul(usize::try_from(height).unwrap())
            .unwrap()
            .checked_mul(4)
            .unwrap();
        let mut pixels = Vec::with_capacity(len);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&pixel(x, y));
            }
        }
        Snapshot::from_rgba(width, height, pixels, Opacity::Source)
    }

    fn mixed(x: u32, y: u32) -> [u8; 4] {
        let alpha = match (x + y) % 3 {
            0 => 0,
            1 => 128,
            _ => 255,
        };
        [alpha / 2, alpha / 3, alpha / 4, alpha]
    }

    fn opaque(x: u32, y: u32) -> [u8; 4] {
        let red = u8::try_from(x).unwrap().wrapping_mul(37);
        let green = u8::try_from(y).unwrap().wrapping_mul(53);
        let blue = red.wrapping_add(green).wrapping_mul(3);
        [red, green, blue, 255]
    }

    fn background(width: i32, height: i32) -> Vec<u8> {
        vec![0x55; usize::try_from(width).unwrap() * usize::try_from(height).unwrap() * 3]
    }

    fn direct(snapshot: &Snapshot) -> SurfaceGeometry {
        SurfaceGeometry::new(
            snapshot,
            1,
            Transform::Normal,
            ViewportCachedState::default(),
        )
    }

    fn compose_direct(
        snapshot: &Snapshot,
        geometry: &SurfaceGeometry,
        origin: Point<i32, Logical>,
        size: (i32, i32),
    ) -> Vec<u8> {
        let mut pixels = background(size.0, size.1);
        {
            let mut target = Target {
                pixels: &mut pixels,
                columns: 0..size.0,
                rows: 0..size.1,
            };
            blit(snapshot, geometry, origin, &mut target, 1.0);
        }
        pixels
    }

    fn compose_scaled(
        snapshot: &Snapshot,
        geometry: &SurfaceGeometry,
        origin: Point<i32, Logical>,
        size: (i32, i32),
    ) -> Vec<u8> {
        let mut pixels = background(size.0, size.1);
        let Some(dst) = placement(origin, geometry.dst, 1.0) else {
            return pixels;
        };
        let left = dst.left.max(0);
        let top = dst.top.max(0);
        let right = dst.left.saturating_add(dst.width).min(size.0);
        let bottom = dst.top.saturating_add(dst.height).min(size.1);
        if right > left && bottom > top {
            let clip = Rectangle::new((left, top).into(), (right - left, bottom - top).into());
            let mut target = Target {
                pixels: &mut pixels,
                columns: 0..size.0,
                rows: 0..size.1,
            };
            if snapshot.opaque() {
                blit_scaled::<true>(snapshot, geometry, dst, clip, &mut target);
            } else {
                blit_scaled::<false>(snapshot, geometry, dst, clip, &mut target);
            }
        }
        pixels
    }

    #[test]
    fn blending_matches_a_rounded_reference_formula() {
        for alpha in 0..=u8::MAX {
            for source in 0..=u8::MAX {
                for under in 0..=u8::MAX {
                    let underneath = [under, under.wrapping_add(40), under.wrapping_sub(50)];
                    let source_rgb = [source, source.wrapping_mul(5), source.wrapping_add(77)];
                    let mut rgb = underneath;
                    blend_pixel(
                        [source_rgb[0], source_rgb[1], source_rgb[2], alpha],
                        &mut rgb,
                    );
                    for channel in 0..3 {
                        let expected = match alpha {
                            0 => underneath[channel],
                            255 => source_rgb[channel],
                            _ => {
                                let alpha = u32::from(alpha);
                                let numerator = u32::from(source_rgb[channel]) * 255
                                    + u32::from(underneath[channel]) * (255 - alpha)
                                    + 127;
                                u8::try_from((numerator / 255).min(255)).unwrap()
                            }
                        };
                        assert_eq!(
                            rgb[channel], expected,
                            "alpha {alpha}, source {source}, under {under}, channel {channel}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn opaque_direct_pixels_copy_exactly() {
        let snapshot = snapshot(2, 2, opaque);
        let geometry = direct(&snapshot);
        let pixels = compose_direct(&snapshot, &geometry, (0, 0).into(), (2, 2));

        assert_eq!(pixels, vec![0, 0, 0, 37, 0, 111, 0, 53, 159, 37, 53, 14]);
    }

    #[test]
    fn unscaled_and_sampled_paths_agree_at_scale_one() {
        let mixed = snapshot(5, 4, mixed);
        let opaque = snapshot(5, 4, opaque);
        let origins = [(-3, -2), (0, 0), (2, 1), (4, 3), (6, 4)];

        for snapshot in [&mixed, &opaque] {
            let geometry = direct(snapshot);
            for origin in origins {
                let origin = Point::from(origin);
                let direct_pixels = compose_direct(snapshot, &geometry, origin, (7, 5));
                let sampled_pixels = compose_scaled(snapshot, &geometry, origin, (7, 5));
                assert_eq!(
                    direct_pixels,
                    sampled_pixels,
                    "origin {origin:?}, opaque {}",
                    snapshot.opaque()
                );
            }
        }
    }

    #[test]
    fn opaque_top_layer_makes_partial_recomposition_match_full_composition() {
        let size = (6, 4);
        let bottom = snapshot(6, 4, mixed);
        let top = snapshot(6, 4, opaque);
        let bottom_geometry = direct(&bottom);
        let top_geometry = direct(&top);

        let mut full = compose_direct(&bottom, &bottom_geometry, (0, 0).into(), size);
        {
            let mut target = Target {
                pixels: &mut full,
                columns: 0..size.0,
                rows: 0..size.1,
            };
            blit(&top, &top_geometry, (0, 0).into(), &mut target, 1.0);
        }
        let shortcut = compose_direct(&top, &top_geometry, (0, 0).into(), size);
        assert_eq!(full, shortcut);

        let partial = snapshot(3, 2, opaque);
        let partial_geometry = direct(&partial);
        let shortcut = compose_direct(&partial, &partial_geometry, (0, 0).into(), size);
        assert_ne!(full, shortcut);
    }

    fn pixel_at(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
        let index = (y * width + x) * 3;
        pixels[index..index + 3].try_into().unwrap()
    }

    fn compose_band(bottom: &Snapshot, top: &Snapshot, region: Rectangle<i32, Logical>) -> Vec<u8> {
        let width = usize::try_from(region.size.w).unwrap();
        let height = usize::try_from(region.size.h).unwrap();
        let mut pixels = vec![0x55; width * height * 3];
        {
            let mut target = Target {
                pixels: &mut pixels,
                columns: region.loc.x..region.loc.x + region.size.w,
                rows: region.loc.y..region.loc.y + region.size.h,
            };
            blit(bottom, &direct(bottom), (0, 0).into(), &mut target, 1.0);
            blit(top, &direct(top), (0, 0).into(), &mut target, 1.0);
        }
        pixels
    }

    fn patch_band(
        frame: &mut [u8],
        frame_width: i32,
        band: &[u8],
        region: Rectangle<i32, Logical>,
    ) {
        let band_width = usize::try_from(region.size.w).unwrap() * 3;
        for row in 0..region.size.h {
            let from = usize::try_from(row).unwrap() * band_width;
            let to =
                usize::try_from((region.loc.y + row) * frame_width + region.loc.x).unwrap() * 3;
            frame[to..to + band_width].copy_from_slice(&band[from..from + band_width]);
        }
    }

    #[test]
    fn viewport_crops_and_scales_samples() {
        let source = snapshot(4, 2, |x, y| {
            [
                u8::try_from(x).unwrap() * 10,
                u8::try_from(y).unwrap() * 20,
                0,
                255,
            ]
        });
        let viewport = ViewportCachedState {
            src: Some(Rectangle::new((1.0, 0.0).into(), (2.0, 2.0).into())),
            dst: Some(Size::from((4, 4))),
        };
        let geometry = SurfaceGeometry::new(&source, 1, Transform::Normal, viewport);
        let pixels = compose_scaled(&source, &geometry, (0, 0).into(), (4, 4));

        assert_eq!(pixel_at(&pixels, 4, 0, 0), [10, 0, 0]);
        assert_eq!(pixel_at(&pixels, 4, 3, 0), [20, 0, 0]);
        assert_eq!(pixel_at(&pixels, 4, 0, 1), [10, 0, 0]);
        assert_eq!(pixel_at(&pixels, 4, 0, 2), [10, 20, 0]);
        assert_eq!(pixel_at(&pixels, 4, 3, 3), [20, 20, 0]);
    }

    #[test]
    fn rotated_buffers_sample_in_transform_order() {
        let source = snapshot(2, 1, |x, _| {
            if x == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 255, 0, 255]
            }
        });
        let geometry =
            SurfaceGeometry::new(&source, 1, Transform::_90, ViewportCachedState::default());
        assert_eq!(geometry.dst, Size::from((1, 2)));
        let pixels = compose_scaled(&source, &geometry, (0, 0).into(), (1, 2));

        assert_eq!(pixel_at(&pixels, 1, 0, 0), [0, 255, 0]);
        assert_eq!(pixel_at(&pixels, 1, 0, 1), [255, 0, 0]);
    }

    proptest! {
        #[test]
        fn damage_limited_recomposition_matches_a_full_recomposition(
            damage in (0i32..6, 0i32..4).prop_flat_map(|(x, y)| {
                (Just(x), Just(y), 1..=(6 - x), 1..=(4 - y))
                    .prop_map(|(x, y, width, height)| {
                        Rectangle::new((x, y).into(), (width, height).into())
                    })
            })
        ) {
            let bottom = snapshot(6, 4, mixed);
            let changed = |x: u32, y: u32| {
                let x = i32::try_from(x).unwrap();
                let y = i32::try_from(y).unwrap();
                x >= damage.loc.x
                    && x < damage.loc.x + damage.size.w
                    && y >= damage.loc.y
                    && y < damage.loc.y + damage.size.h
            };
            let old_top = snapshot(6, 4, |x, y| {
                if changed(x, y) {
                    [0, 0, 0, 0]
                } else {
                    [10, 200, 30, 64]
                }
            });
            let new_top = snapshot(6, 4, |x, y| {
                if changed(x, y) {
                    [200, 10, 20, 128]
                } else {
                    [10, 200, 30, 64]
                }
            });
            let full = Rectangle::new((0, 0).into(), (6, 4).into());

            let old_frame = compose_band(&bottom, &old_top, full);
            let new_frame = compose_band(&bottom, &new_top, full);
            let mut incremental = old_frame.clone();
            let band = compose_band(&bottom, &new_top, damage);
            patch_band(&mut incremental, 6, &band, damage);

            prop_assert_ne!(&old_frame, &new_frame, "damage {:?}", damage);
            prop_assert_eq!(&incremental, &new_frame, "damage {:?}", damage);
        }
    }

    fn assert_placement(got: Option<Placement>, want: (i32, i32, i32, i32), description: &str) {
        let got = got.unwrap_or_else(|| panic!("{description}: no placement"));
        assert_eq!(
            (got.left, got.top, got.width, got.height),
            want,
            "{description}"
        );
    }

    #[test]
    fn placement_rounds_scaled_edges() {
        assert_placement(
            placement(Point::from((1, 2)), Size::from((3, 4)), 1.0),
            (1, 2, 3, 4),
            "unscaled",
        );
        assert_placement(
            placement(Point::from((1, 1)), Size::from((3, 3)), 0.5),
            (1, 1, 1, 1),
            "half-scale offset",
        );
        assert_placement(
            placement(Point::from((0, 0)), Size::from((3, 3)), 0.5),
            (0, 0, 2, 2),
            "half-scale rounding",
        );
        assert_placement(
            placement(Point::from((0, 0)), Size::from((1, 1)), 1.5),
            (0, 0, 2, 2),
            "edge rounding",
        );
    }

    #[test]
    fn placement_rejects_empty_or_saturated_destinations() {
        assert!(
            placement(Point::from((0, 0)), Size::from((0, 4)), 1.0).is_none(),
            "zero-width destination"
        );
        assert!(
            placement(Point::from((i32::MAX, i32::MAX)), Size::from((10, 10)), 2.0).is_none(),
            "saturated destination"
        );
    }
}
