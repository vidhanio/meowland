//! Drawing a pane's frame, and answering what a point lands on.

use std::collections::HashMap;

use smithay::{
    desktop::PopupManager,
    reexports::wayland_server::{
        Resource as _, backend::ObjectId, protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{
            SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction, get_parent,
            with_states, with_surface_tree_downward,
        },
        shell::xdg::SurfaceCachedState,
        viewporter::ViewportCachedState,
    },
};

use super::Compositor;
use crate::{
    render::{Frame, Image, Rect},
    wayland::buffer::Snapshot,
};

const BACKDROP: [u8; 3] = [0x14, 0x16, 0x1b];

impl Compositor {
    /// Repaint one pane from the window it shows.
    ///
    /// Reading geometry needs `&self` and drawing needs the frame, so the
    /// drawing order is planned first and painted afterwards.
    pub(super) fn draw(&mut self, pane: usize, frame: &mut Frame) {
        self.plan.clear();
        let bounds = frame.bounds();
        let mut covered = false;
        if let Some(window) = self.views[pane].index(&self.windows) {
            let surface = self.windows[window].surface.wl_surface().clone();
            if let Some(snapshot) = self.snapshots.get(&surface.id()) {
                let (src, size) = with_states(&surface, |states| placement(states, snapshot));
                covered = covers(bounds, &snapshot.image(), src, size);
                self.plan.push((surface.clone(), Point::from((0, 0))));
                for (popup, origin) in popup_origins(&surface) {
                    self.plan.push((popup, origin));
                }
            }
        }
        // A window that covers the pane with opaque pixels leaves no backdrop
        // to be seen, so there is none to lay down under it: a screen's worth
        // of pixels is written once per frame instead of twice.
        if !covered {
            frame.clear(BACKDROP);
        }

        for (surface, position) in &self.plan {
            draw_tree(frame, &self.snapshots, surface, *position);
        }
    }

    /// The surface under a point, in the coordinates of the window the pane
    /// shows.
    pub(super) fn surface_at(
        &self,
        pane: usize,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        let window = self.views[pane].index(&self.windows)?;
        if !Rect::new(
            0,
            0,
            self.views[pane].capabilities.pixels.0,
            self.views[pane].capabilities.pixels.1,
        )
        .contains(point.x as i32, point.y as i32)
        {
            return None;
        }
        let surface = self.windows[window].surface.wl_surface().clone();
        for (popup, origin) in popup_origins(&surface) {
            if let Some(under) = surface_under(&self.snapshots, &popup, point, origin) {
                return Some(under);
            }
        }
        surface_under(&self.snapshots, &surface, point, Point::from((0, 0)))
    }
}

/// Where each of a window's popups is drawn, relative to the same origin the
/// window's own surface is drawn from.
///
/// A popup arrives at a location relative to the window geometry the client
/// set, and what a pane draws from is the surface's own origin, so this is
/// where the two are reconciled. Drawing the pane and answering what a point
/// lands on have to agree about it, which is why they ask the same function.
fn popup_origins(
    surface: &WlSurface,
) -> impl Iterator<Item = (WlSurface, Point<i32, Logical>)> + use<'_> {
    let geometry = geometry_offset(surface);
    PopupManager::popups_for_surface(surface).map(move |(popup, location)| {
        (
            popup.wl_surface().clone(),
            geometry + location - popup.geometry().loc,
        )
    })
}

/// Whether the pane that draws the window rooted at `root` also draws
/// `surface`.
///
/// That is the window itself, a sub-surface of it, or a popup of either: what
/// [`Compositor::draw`] puts in a pane's frame, and nothing else. A client's
/// cursor surface, which commits on every mouse move, is none of them.
pub(super) fn is_drawn(root: &WlSurface, surface: &WlSurface) -> bool {
    below(root, surface)
        || PopupManager::popups_for_surface(root)
            .any(|(popup, _)| below(popup.wl_surface(), surface))
}

/// Whether `surface` is `root` or below it in its sub-surface tree.
fn below(root: &WlSurface, surface: &WlSurface) -> bool {
    let mut current = surface.clone();
    loop {
        if &current == root {
            return true;
        }
        match get_parent(&current) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

/// Where the toplevel's window geometry starts, relative to its surface origin.
///
/// Popup positions arrive relative to that rectangle.
fn geometry_offset(surface: &WlSurface) -> Point<i32, Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map_or_else(|| (0, 0).into(), |geometry| geometry.loc)
    })
}

/// Blend a surface tree into the frame, with the tree's root at `location`.
///
/// Both traversal closures receive the *parent's* accumulated location, so each
/// one adds its own offset.
fn draw_tree(
    frame: &mut Frame,
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    location: Point<i32, Logical>,
) {
    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| TraversalAction::DoChildren(*location + subsurface_offset(states)),
        |surface, states, location| {
            draw_surface(
                frame,
                snapshots,
                surface,
                states,
                *location + subsurface_offset(states),
            );
        },
        |_, _, _| true,
    );
}

/// Blend one surface of a tree into the frame, from its committed copy.
fn draw_surface(
    frame: &mut Frame,
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    states: &SurfaceData,
    location: Point<i32, Logical>,
) {
    // A surface with nothing committed, or with a detached buffer, draws
    // nothing.
    let Some(snapshot) = snapshots.get(&surface.id()) else {
        return;
    };
    let (src, (width, height)) = placement(states, snapshot);
    frame.draw(
        &snapshot.image(),
        src,
        Rect::new(location.x, location.y, width, height),
    );
}

/// Which pixels of a surface's committed copy are drawn, and how large the
/// picture is: the viewport's source rectangle, and the size it takes.
///
/// Both are what `wp_viewporter` asks for, and both are in whole buffer pixels
/// when they came from the viewport: a source rectangle covers as many pixels
/// as it names, and a destination is at least one pixel either way.
fn placement(states: &SurfaceData, snapshot: &Snapshot) -> (Rect, (u32, u32)) {
    let viewport = *states.cached_state.get::<ViewportCachedState>().current();
    let scale = f64::from(snapshot.scale.max(1));
    let src = viewport.src.map_or_else(
        || Rect::new(0, 0, snapshot.width, snapshot.height),
        |src| {
            Rect::new(
                (src.loc.x * scale).floor() as i32,
                (src.loc.y * scale).floor() as i32,
                (src.size.w * scale).ceil() as u32,
                (src.size.h * scale).ceil() as u32,
            )
        },
    );
    (src, drawn_size(states, snapshot))
}

/// How large a surface's committed copy is drawn: the viewport's size when the
/// client set one, and the buffer's own logical size otherwise.
///
/// A surface is drawn at this size and takes input over it, so both ask here.
fn drawn_size(states: &SurfaceData, snapshot: &Snapshot) -> (u32, u32) {
    let (width, height) = states
        .cached_state
        .get::<ViewportCachedState>()
        .current()
        .size()
        .map_or_else(
            || snapshot.logical_size(),
            |size| (size.w.max(1), size.h.max(1)),
        );
    (width as u32, height as u32)
}

/// Whether a surface's committed copy covers the whole of `bounds`, drawn where
/// the pane draws a window's own surface.
///
/// That takes all three: no alpha to blend with, no crop of the buffer, and a
/// destination no smaller than the frame. A surface that does leaves no
/// backdrop to be seen under it, and the frame does not have to be laid down
/// with one.
fn covers(bounds: Rect, image: &Image<'_>, src: Rect, size: (u32, u32)) -> bool {
    image.format.opaque()
        && src == Rect::new(0, 0, image.width, image.height)
        && size.0 >= bounds.width
        && size.1 >= bounds.height
}

fn subsurface_offset(states: &SurfaceData) -> Point<i32, Logical> {
    states
        .cached_state
        .get::<SubsurfaceCachedState>()
        .current()
        .location
}

/// The topmost surface of a tree that a point hits, in the client's
/// coordinates.
///
/// The point arrives in output coordinates and `origin` is a screen position. A
/// surface accepts input only inside its input region; with no region, the
/// whole surface accepts it.
fn surface_under(
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    point: Point<f64, Logical>,
    origin: Point<i32, Logical>,
) -> Option<(WlSurface, Point<i32, Logical>)> {
    use std::cell::RefCell;

    let found = RefCell::new(None);
    // Downward order is topmost first, the order a click must be matched in.
    with_surface_tree_downward(
        surface,
        origin,
        |_, states, location| TraversalAction::DoChildren(*location + subsurface_offset(states)),
        |surface, states, location| {
            if found.borrow().is_some() {
                return;
            }
            let location = *location + subsurface_offset(states);
            let Some(snapshot) = snapshots.get(&surface.id()) else {
                return;
            };
            let size = drawn_size(states, snapshot);
            if accepts_input(
                states,
                point - location.to_f64(),
                (size.0 as i32, size.1 as i32),
            ) {
                *found.borrow_mut() = Some((surface.clone(), location));
            }
        },
        |_, _, _| found.borrow().is_none(),
    );
    found.into_inner()
}

/// Whether a surface accepts input at a point, given in that surface's own
/// coordinates.
///
/// A client can cut its sensitive area out with `wl_surface.set_input_region`;
/// with no region the whole surface counts, as the protocol says.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the input region is borrowed out of the cached state, so the guard outlives the check"
)]
fn accepts_input(states: &SurfaceData, local: Point<f64, Logical>, size: (i32, i32)) -> bool {
    let mut state = states.cached_state.get::<SurfaceAttributes>();
    let attributes = state.current();
    attributes.input_region.as_ref().map_or_else(
        || {
            local.x >= 0.0
                && local.y >= 0.0
                && local.x < f64::from(size.0)
                && local.y < f64::from(size.1)
        },
        |region| region.contains((local.x.floor() as i32, local.y.floor() as i32)),
    )
}

/// Send the frame callbacks a surface tree waits for, and clear them.
///
/// A client driven by animation draws one frame per callback, so this paces it.
pub(super) fn send_frame_callbacks(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, ()| TraversalAction::DoChildren(()),
        |_, states, ()| {
            let mut state = states.cached_state.get::<SurfaceAttributes>();
            for callback in state.current().frame_callbacks.drain(..) {
                callback.done(time);
            }
        },
        |_, _, ()| true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{BYTES4, SourceFormat};

    /// A pane of 80 by 64 pixels.
    const BOUNDS: Rect = Rect::new(0, 0, 80, 64);

    /// The whole of a buffer of this size, which is what a covering window
    /// draws from.
    const fn whole(width: u32, height: u32) -> Rect {
        Rect::new(0, 0, width, height)
    }

    /// A buffer of this shape, in this layout. Nothing reads its pixels: what
    /// `covers` asks about is the shape, not what is in it.
    fn image(format: SourceFormat, width: u32, height: u32) -> Image<'static> {
        Image {
            pixels: &[],
            stride: width as usize * BYTES4,
            width,
            height,
            format,
        }
    }

    fn covers_from(format: SourceFormat, buffer: (u32, u32), src: Rect, size: (u32, u32)) -> bool {
        covers(BOUNDS, &image(format, buffer.0, buffer.1), src, size)
    }

    #[test]
    fn a_window_that_fills_the_pane_covers_it() {
        // What a video or a browser fills a pane with: the whole of its buffer,
        // no alpha, and the pane's own size or more.
        assert!(covers_from(
            SourceFormat::Xrgb8888,
            (80, 64),
            whole(80, 64),
            (80, 64)
        ));
        assert!(covers_from(
            SourceFormat::Xrgb8888,
            (80, 64),
            whole(80, 64),
            (160, 128)
        ));
        assert!(covers_from(
            SourceFormat::Xbgr8888,
            (160, 128),
            whole(160, 128),
            (80, 64)
        ));
    }

    #[test]
    fn a_window_that_leaves_the_backdrop_visible_does_not_cover() {
        // Alpha: whatever the pixels say, the backdrop shows through it.
        assert!(!covers_from(
            SourceFormat::Argb8888,
            (80, 64),
            whole(80, 64),
            (80, 64)
        ));
        // A crop of the buffer: the pixels outside the source keep what the
        // frame had.
        assert!(!covers_from(
            SourceFormat::Xrgb8888,
            (80, 64),
            Rect::new(8, 0, 72, 64),
            (80, 64)
        ));
        // Neither way round is a size smaller than the pane.
        assert!(!covers_from(
            SourceFormat::Xrgb8888,
            (80, 64),
            whole(80, 64),
            (79, 64)
        ));
        assert!(!covers_from(
            SourceFormat::Xrgb8888,
            (80, 64),
            whole(80, 64),
            (80, 63)
        ));
    }
}
