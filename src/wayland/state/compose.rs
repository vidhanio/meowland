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
    render::{Frame, Rect},
    wayland::buffer::Snapshot,
};

const BACKDROP: [u8; 3] = [0x14, 0x16, 0x1b];

impl Compositor {
    /// Repaint one pane from the window it shows.
    ///
    /// Reading geometry needs `&self` and drawing needs the frame, so the
    /// drawing order is planned first and painted afterwards.
    pub(super) fn draw(&mut self, pane: usize, frame: &mut Frame) {
        frame.clear(BACKDROP);

        self.plan.clear();
        if let Some(window) = self.views[pane].index(&self.windows) {
            let surface = self.windows[window].surface.wl_surface().clone();
            if self.snapshots.contains_key(&surface.id()) {
                self.plan.push((surface.clone(), Point::from((0, 0))));
                let geometry = geometry_offset(&surface);
                for (popup, location) in PopupManager::popups_for_surface(&surface) {
                    self.plan.push((
                        popup.wl_surface().clone(),
                        geometry + location - popup.geometry().loc,
                    ));
                }
            }
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
        let geometry = geometry_offset(&surface);
        for (popup, location) in PopupManager::popups_for_surface(&surface) {
            let origin = geometry + location - popup.geometry().loc;
            if let Some(under) = surface_under(&self.snapshots, popup.wl_surface(), point, origin) {
                return Some(under);
            }
        }
        surface_under(&self.snapshots, &surface, point, Point::from((0, 0)))
    }
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
    let (width, height) = viewport.size().map_or_else(
        || snapshot.logical_size(),
        |size| (size.w.max(1), size.h.max(1)),
    );
    frame.draw(
        &snapshot.image(),
        src,
        Rect::new(location.x, location.y, width as u32, height as u32),
    );
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
            let viewport = *states.cached_state.get::<ViewportCachedState>().current();
            let size = viewport.size().map_or_else(
                || snapshot.logical_size(),
                |size| (size.w.max(1), size.h.max(1)),
            );
            if accepts_input(states, point - location.to_f64(), size) {
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
