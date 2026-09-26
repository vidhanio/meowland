//! Surface traversal, hit testing, and frame composition.

use super::{
    Logical, Point, PopupKind, PopupManager, Rectangle, Size, Snapshot, State,
    SubsurfaceCachedState, SurfaceAttributes, SurfaceCachedState, ViewportCachedState, WlSurface,
    get_children, with_states,
};

/// Compose one pane's frame into [`State::scratch`], reporting whether there
/// was anything to compose: the window it shows, its subsurfaces and its
/// popups, over an opaque backdrop.
pub(super) fn render_frame(state: &mut State, pane: u64) -> bool {
    let Some((window, width, height)) = state
        .panes
        .get(&pane)
        .map(|entry| (entry.window, entry.width, entry.height))
    else {
        return false;
    };
    // Drawing and the backdrop decision share one stack, so they cannot
    // disagree about what is on the pane.
    let stack = state
        .windows
        .get(&window)
        .map_or_default(|entry| surface_stack(&entry.surface.wl_surface().clone()));
    let mut out = std::mem::take(&mut state.scratch);
    out.resize(width as usize * height as usize * 3, 0);
    if !opaque_cover(state, &stack, width, height) {
        out.fill(0);
    }
    for (surface, origin) in &stack {
        blit(state, surface, *origin, &mut out, width, height);
    }
    state.scratch = out;
    true
}

/// Every surface a window draws, bottom to top, with the pane coordinates of
/// its top-left corner.  Drawing and hit-testing share this list, so they
/// cannot disagree about where a surface is.
pub(super) fn surface_stack(root: &WlSurface) -> Vec<(WlSurface, Point<i32, Logical>)> {
    let mut stack = Vec::new();
    collect_surface(root, (0, 0).into(), &mut stack);
    let geometry = window_geometry(root);
    let mut popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
    // The manager lists children before their parents; drawing runs bottom to
    // top, so parents and older siblings come first.
    popups.reverse();
    for (popup, location) in popups {
        let origin = popup_origin(geometry, location, &popup);
        collect_surface(popup.wl_surface(), origin, &mut stack);
    }
    stack
}

/// The origin of the window geometry a client set on a surface, which is what
/// a popup's position is measured against.
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

/// Where a popup's surface starts in pane coordinates.  `location` is where
/// the popup sits against the parent's window geometry, and the popup's own
/// window geometry sits inside the surface it draws from, so it comes back
/// off: a pane draws a surface from its own origin, not from its geometry.
pub(super) fn popup_origin(
    parent_geometry: Point<i32, Logical>,
    location: Point<i32, Logical>,
    popup: &PopupKind,
) -> Point<i32, Logical> {
    parent_geometry + location - popup.geometry().loc
}

pub(super) fn collect_surface(
    surface: &WlSurface,
    origin: Point<i32, Logical>,
    stack: &mut Vec<(WlSurface, Point<i32, Logical>)>,
) {
    stack.push((surface.clone(), origin));
    for child in get_children(surface) {
        let offset = with_states(&child, |states| {
            states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location
        });
        collect_surface(&child, origin + offset, stack);
    }
}

/// Copy one surface's snapshot into a pane, applying its viewport and
/// blending premultiplied alpha over what is already there.
pub(super) fn blit(
    state: &State,
    surface: &WlSurface,
    origin: Point<i32, Logical>,
    out: &mut [u8],
    width: u32,
    height: u32,
) {
    let Some(snapshot) = state.snapshots.get(surface) else {
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
    // Both extents are client-chosen, so the box is clamped, not added.
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
        // No viewport: one pane pixel per snapshot pixel, which is what a
        // client that never uses `wp_viewporter` draws at.
        blit_unscaled(snapshot, left, top, clip, out, width);
        return;
    }
    let last_x = snapshot.width as usize - 1;
    let last_y = snapshot.height as usize - 1;
    let stride = snapshot.width as usize * 4;
    for y in clip_top..clip_bottom {
        let sample = src.loc.y + (f64::from(y - top) + 0.5) * src.size.h / f64::from(dst.h);
        let row = (sample.floor().max(0.0) as usize).min(last_y) * stride;
        let to = (y as usize * width as usize + clip_left as usize) * 3;
        for x in clip_left..clip_right {
            let sample = src.loc.x + (f64::from(x - left) + 0.5) * src.size.w / f64::from(dst.w);
            let column = (sample.floor().max(0.0) as usize).min(last_x);
            let pixel = row + column * 4;
            let at = to + (x - clip_left) as usize * 3;
            if snapshot.opaque {
                out[at..at + 3].copy_from_slice(&snapshot.pixels[pixel..pixel + 3]);
                continue;
            }
            let alpha = u32::from(snapshot.pixels[pixel + 3]);
            match alpha {
                0 => {}
                255 => out[at..at + 3].copy_from_slice(&snapshot.pixels[pixel..pixel + 3]),
                _ => {
                    let inverse = 255 - alpha;
                    for channel in 0..3 {
                        let under = u32::from(out[at + channel]);
                        out[at + channel] = (u32::from(snapshot.pixels[pixel + channel])
                            + (under * inverse + 127) / 255)
                            as u8;
                    }
                }
            }
        }
    }
}

/// The unscaled case of [`blit`]: the surface has no viewport, so one pane
/// pixel is one snapshot pixel and the sample of a pixel is its offset from
/// the surface origin.  Integer indexing alone, with the alpha blend kept
/// byte for byte identical to the sampling path's.
pub(super) fn blit_unscaled(
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
    // The clip was taken against the snapshot's own size, so `first` and
    // `first_row` are inside it and every span below is in bounds.
    let first = (clip.loc.x - left) as usize;
    let first_row = (clip.loc.y - top) as usize;
    for row in 0..clip.size.h as usize {
        let from = (first_row + row) * stride + first * 4;
        let source = snapshot.pixels[from..from + row_bytes].as_chunks::<4>().0;
        let at = (clip.loc.y as usize + row) * pane_stride + clip.loc.x as usize * 3;
        let dest = out[at..at + columns * 3].as_chunks_mut::<3>().0;
        if snapshot.opaque {
            for (pixel, rgb) in source.iter().zip(dest) {
                rgb.copy_from_slice(&pixel[..3]);
            }
            continue;
        }
        for (pixel, rgb) in source.iter().zip(dest) {
            let alpha = u32::from(pixel[3]);
            match alpha {
                0 => {}
                255 => rgb.copy_from_slice(&pixel[..3]),
                _ => {
                    let inverse = 255 - alpha;
                    for channel in 0..3 {
                        let under = u32::from(rgb[channel]);
                        rgb[channel] =
                            (u32::from(pixel[channel]) + (under * inverse + 127) / 255) as u8;
                    }
                }
            }
        }
    }
}

/// Whether the topmost thing a window draws is opaque and covers the whole
/// pane, in which case the backdrop under it need not be laid down.
pub(super) fn opaque_cover(
    state: &State,
    stack: &[(WlSurface, Point<i32, Logical>)],
    width: u32,
    height: u32,
) -> bool {
    let Some((surface, origin)) = stack.last() else {
        return false;
    };
    let Some(snapshot) = state.snapshots.get(surface) else {
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

/// The source rectangle and on-screen size of a surface after its viewport.
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

/// The surface under a point, topmost first, with the pane coordinates of its
/// top-left corner.  Smithay wants the surface origin, not the local point:
/// it subtracts the origin itself before telling the client where it is.
pub(super) fn hit_test(
    state: &State,
    window: u64,
    x: f64,
    y: f64,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    let root = state.windows.get(&window)?.surface.wl_surface().clone();
    for (surface, origin) in surface_stack(&root).into_iter().rev() {
        let Some(snapshot) = state.snapshots.get(&surface) else {
            continue;
        };
        let (_, dst) = extents(snapshot, viewport_of(&surface));
        let local = Point::<f64, Logical>::from((x - f64::from(origin.x), y - f64::from(origin.y)));
        if local.x < 0.0
            || local.y < 0.0
            || local.x >= f64::from(dst.w)
            || local.y >= f64::from(dst.h)
        {
            continue;
        }
        let region = with_states(&surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .input_region
                .clone()
        });
        let inside = region
            .is_none_or(|region| region.contains((local.x.floor() as i32, local.y.floor() as i32)));
        if inside {
            return Some((
                surface,
                Point::from((f64::from(origin.x), f64::from(origin.y))),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscaled_blit_clips_and_blends_premultiplied_alpha() {
        let snapshot = Snapshot {
            width: 3,
            height: 1,
            pixels: vec![
                255, 0, 0, 255, // clipped away
                0, 64, 0, 128, // half-transparent green
                0, 0, 0, 0, // transparent
            ],
            opaque: false,
        };
        let mut pane = vec![10, 20, 30, 40, 50, 60];
        let clip = Rectangle::new((0, 0).into(), (2, 1).into());

        blit_unscaled(&snapshot, -1, 0, clip, &mut pane, 2);

        assert_eq!(pane, [5, 74, 15, 40, 50, 60]);
    }
}
