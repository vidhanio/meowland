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
pub fn surface_geometry(surface: &WlSurface, snapshot: &Snapshot) -> SurfaceGeometry {
    with_states(surface, |states| {
        let (scale, transform) = {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let properties = (
                current.buffer_scale,
                Transform::from(current.buffer_transform),
            );
            drop(attributes);
            properties
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

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "rounding and saturating to the Wayland coordinate range are intentional"
)]
pub(crate) fn rounded(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use smithay::{
        utils::{Rectangle, Size, Transform},
        wayland::viewporter::ViewportCachedState,
    };

    use super::*;
    use crate::snapshot::Opacity;

    fn snapshot(width: u32, height: u32) -> Snapshot {
        let len = usize::try_from(width)
            .unwrap()
            .checked_mul(usize::try_from(height).unwrap())
            .unwrap()
            .checked_mul(4)
            .unwrap();
        Snapshot::from_rgba(width, height, vec![0; len], Opacity::Source)
    }

    fn geometry(
        snapshot: &Snapshot,
        scale: i32,
        transform: Transform,
        viewport: ViewportCachedState,
    ) -> SurfaceGeometry {
        SurfaceGeometry::new(snapshot, scale, transform, viewport)
    }

    #[test]
    fn new_derives_the_full_buffer_without_a_viewport() {
        let snapshot = snapshot(8, 4);
        let geometry = geometry(
            &snapshot,
            1,
            Transform::Normal,
            ViewportCachedState::default(),
        );
        let buffer: Size<f64, Logical> = (8.0, 4.0).into();

        assert_eq!(geometry.buffer, buffer);
        assert_eq!(geometry.src, Rectangle::from_size(buffer));
        assert_eq!(geometry.dst, Size::from((8, 4)));
        assert_eq!(geometry.scale, 1);
        assert_eq!(geometry.transform, Transform::Normal);
        assert!(geometry.direct());
        assert!(!geometry.swapped());
    }

    #[test]
    fn buffer_scale_shrinks_the_logical_buffer() {
        let snapshot = snapshot(8, 4);
        let geometry = geometry(
            &snapshot,
            2,
            Transform::Normal,
            ViewportCachedState::default(),
        );

        assert_eq!(geometry.buffer, Size::from((4.0, 2.0)));
        assert_eq!(geometry.dst, Size::from((4, 2)));
        assert_eq!(geometry.scale, 2);
        assert!(!geometry.direct());
    }

    #[test]
    fn nonpositive_scale_is_treated_as_one() {
        let snapshot = snapshot(8, 4);
        for scale in [0, -1, i32::MIN] {
            let geometry = geometry(
                &snapshot,
                scale,
                Transform::Normal,
                ViewportCachedState::default(),
            );
            assert_eq!(geometry.buffer, Size::from((8.0, 4.0)), "scale {scale}");
            assert_eq!(geometry.scale, 1, "scale {scale}");
            assert_eq!(geometry.dst, Size::from((8, 4)), "scale {scale}");
            assert!(geometry.direct(), "scale {scale}");
        }
    }

    #[test]
    fn rotated_buffers_swap_their_dimensions() {
        let snapshot = snapshot(8, 4);
        for transform in [
            Transform::_90,
            Transform::_270,
            Transform::Flipped90,
            Transform::Flipped270,
        ] {
            let geometry = geometry(&snapshot, 1, transform, ViewportCachedState::default());
            assert_eq!(
                geometry.buffer,
                Size::from((4.0, 8.0)),
                "transform {transform:?}"
            );
            assert_eq!(geometry.dst, Size::from((4, 8)), "transform {transform:?}");
            assert!(geometry.swapped(), "transform {transform:?}");
            assert!(!geometry.direct(), "transform {transform:?}");
        }

        for transform in [
            Transform::Normal,
            Transform::_180,
            Transform::Flipped,
            Transform::Flipped180,
        ] {
            let geometry = geometry(&snapshot, 1, transform, ViewportCachedState::default());
            assert_eq!(
                geometry.buffer,
                Size::from((8.0, 4.0)),
                "transform {transform:?}"
            );
            assert!(!geometry.swapped(), "transform {transform:?}");
        }
    }

    #[test]
    fn viewport_source_and_destination_override_the_buffer() {
        let snapshot = snapshot(8, 4);
        let viewport = ViewportCachedState {
            src: Some(Rectangle::new((2.0, 1.0).into(), (4.0, 2.0).into())),
            dst: Some(Size::from((16, 8))),
        };
        let geometry = geometry(&snapshot, 1, Transform::Normal, viewport);

        assert_eq!(
            geometry.src,
            Rectangle::new((2.0, 1.0).into(), (4.0, 2.0).into())
        );
        assert_eq!(geometry.dst, Size::from((16, 8)));
        assert!(!geometry.direct());
    }

    #[test]
    fn a_destination_matching_the_buffer_stays_direct() {
        let snapshot = snapshot(8, 4);
        let viewport = ViewportCachedState {
            src: None,
            dst: Some(Size::from((8, 4))),
        };
        let geometry = geometry(&snapshot, 1, Transform::Normal, viewport);

        assert_eq!(geometry.src, Rectangle::from_size(Size::from((8.0, 4.0))));
        assert_eq!(geometry.dst, Size::from((8, 4)));
        assert!(geometry.direct());
    }

    #[test]
    fn source_bound_applies_scale_and_rotation() {
        assert_eq!(source_bound(None, 1, Transform::Normal), None);
        assert_eq!(
            source_bound(Some((100, 50)), 2, Transform::Normal),
            Some((200, 100))
        );
        assert_eq!(
            source_bound(Some((100, 50)), 2, Transform::_90),
            Some((100, 200))
        );
        assert_eq!(
            source_bound(Some((100, 50)), 0, Transform::Normal),
            Some((100, 50))
        );
        assert_eq!(
            source_bound(Some((100, 50)), -2, Transform::Normal),
            Some((100, 50))
        );
        assert_eq!(
            source_bound(Some((u32::MAX, 1)), 2, Transform::Normal),
            Some((u32::MAX, 2))
        );
    }

    #[test]
    fn rounded_saturates_at_the_coordinate_limits() {
        assert_eq!(rounded(0.4), 0);
        assert_eq!(rounded(0.5), 1);
        assert_eq!(rounded(-0.5), -1);
        assert_eq!(rounded(1.5), 2);
        assert_eq!(rounded(f64::MAX), i32::MAX);
        assert_eq!(rounded(f64::MIN), i32::MIN);
        assert_eq!(rounded(f64::from(i32::MAX) + 10.0), i32::MAX);
        assert_eq!(rounded(f64::from(i32::MIN) - 10.0), i32::MIN);
    }
}
