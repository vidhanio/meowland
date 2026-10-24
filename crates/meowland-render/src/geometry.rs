//! Buffer-to-surface coordinate geometry for scale, transform, and viewport
//! state.

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor::{SurfaceAttributes, with_states},
        viewporter::ViewportCachedState,
    },
};

use super::Snapshot;

/// A snapshot buffer's mapping into surface-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceGeometry {
    /// Viewport source rectangle in surface coordinates before scaling.
    pub src: Rectangle<f64, Logical>,
    /// Final surface destination size.
    pub dst: Size<i32, Logical>,
    /// Full logical buffer size after transform and scale.
    pub buffer: Size<f64, Logical>,
    /// Buffer scale committed with the surface.
    pub scale: i32,
    /// Buffer transform committed with the surface.
    pub transform: Transform,
}

impl SurfaceGeometry {
    /// Apply committed surface state to a raw snapshot.
    #[must_use]
    pub fn new(
        snapshot: &Snapshot,
        scale: i32,
        transform: Transform,
        viewport: ViewportCachedState,
    ) -> Self {
        let scale = scale.max(1);
        let buffer = transform.invert().transform_size(Size::from((
            f64::from(snapshot.width) / f64::from(scale),
            f64::from(snapshot.height) / f64::from(scale),
        )));
        let full = Rectangle::from_size(buffer);
        let src = viewport.src.unwrap_or(full);
        let dst = viewport
            .dst
            .unwrap_or_else(|| Size::from((rounded(src.size.w), rounded(src.size.h))));
        Self {
            src,
            dst,
            buffer,
            scale,
            transform,
        }
    }

    /// Whether the snapshot can be copied without resampling or reorienting.
    #[must_use]
    pub fn direct(&self) -> bool {
        self.scale == 1
            && self.transform == Transform::Normal
            && self.src == Rectangle::from_size(self.buffer)
            && self.dst == Size::from((rounded(self.buffer.w), rounded(self.buffer.h)))
    }

    pub(crate) const fn swapped(&self) -> bool {
        matches!(
            self.transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        )
    }

    pub(crate) fn raw_x(&self, x: f64) -> f64 {
        self.raw(x, 0.0).x
    }

    pub(crate) fn raw_y(&self, y: f64) -> f64 {
        self.raw(0.0, y).y
    }

    pub(crate) fn raw_x_from_y(&self, y: f64) -> f64 {
        self.raw(0.0, y).x
    }

    pub(crate) fn raw_y_from_x(&self, x: f64) -> f64 {
        self.raw(x, 0.0).y
    }

    fn raw(&self, x: f64, y: f64) -> Point<f64, Logical> {
        let raw = self
            .transform
            .transform_point_in(Point::from((x, y)), &self.buffer);
        Point::from((raw.x * f64::from(self.scale), raw.y * f64::from(self.scale)))
    }
}

/// Read the committed scale, transform, and viewport for a surface.
#[must_use]
#[expect(
    clippy::significant_drop_tightening,
    reason = "`attributes` borrows the cached state and must outlive `current`"
)]
pub fn surface_geometry(surface: &WlSurface, snapshot: &Snapshot) -> SurfaceGeometry {
    with_states(surface, |states| {
        let (scale, transform) = {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            (
                current.buffer_scale,
                Transform::from(current.buffer_transform),
            )
        };
        let viewport = *states.cached_state.get::<ViewportCachedState>().current();
        SurfaceGeometry::new(snapshot, scale, transform, viewport)
    })
}

/// Expand a logical pane bound into a raw buffer bound for the committed
/// scale and transform.
#[must_use]
pub fn source_bound(
    bound: Option<(u32, u32)>,
    scale: i32,
    transform: Transform,
) -> Option<(u32, u32)> {
    let (width, height) = bound?;
    let scale = u32::try_from(scale)
        .ok()
        .filter(|scale| *scale > 0)
        .unwrap_or(1);
    let width = width.saturating_mul(scale);
    let height = height.saturating_mul(scale);
    Some(
        if matches!(
            transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        ) {
            (height, width)
        } else {
            (width, height)
        },
    )
}

fn rounded(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}
