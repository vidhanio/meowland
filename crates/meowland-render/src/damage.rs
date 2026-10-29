//! Retained scene geometry and bounded surface-local damage accumulation.

use std::collections::HashMap;

use meowland_core::pixels::FrameSize;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
    wayland::compositor::Damage,
};

use crate::SurfaceGeometry;

/// Accumulated content changes in final surface-local coordinates.
#[derive(Clone, Copy, Debug, Default)]
pub struct SurfaceDamage(Option<Rectangle<f64, Logical>>);

impl SurfaceDamage {
    /// Convert buffer and surface damage through the committed viewport.
    pub fn new(damage: &[Damage], geometry: &SurfaceGeometry) -> Self {
        let mut result = Self::default();
        let bounds = Rectangle::from_size(geometry.dst.to_f64());
        for damage in damage {
            let rect = match damage {
                Damage::Surface(rect) => rect.to_f64(),
                Damage::Buffer(rect) => {
                    let rect = rect.to_f64().to_logical(
                        f64::from(geometry.scale),
                        geometry.transform,
                        &geometry
                            .buffer
                            .to_buffer(f64::from(geometry.scale), geometry.transform),
                    );
                    let Some(rect) = rect.intersection(geometry.src) else {
                        continue;
                    };
                    Rectangle::new(
                        (
                            (rect.loc.x - geometry.src.loc.x) * f64::from(geometry.dst.w)
                                / geometry.src.size.w,
                            (rect.loc.y - geometry.src.loc.y) * f64::from(geometry.dst.h)
                                / geometry.src.size.h,
                        )
                            .into(),
                        (
                            rect.size.w * f64::from(geometry.dst.w) / geometry.src.size.w,
                            rect.size.h * f64::from(geometry.dst.h) / geometry.src.size.h,
                        )
                            .into(),
                    )
                }
            };
            if let Some(rect) = rect.intersection(bounds) {
                result.add(rect);
            }
        }
        result
    }

    /// Cover a whole surface when its content changed without damage hints.
    pub fn full(geometry: &SurfaceGeometry) -> Self {
        Self(Some(Rectangle::from_size(geometry.dst.to_f64())))
    }

    fn add(&mut self, rect: Rectangle<f64, Logical>) {
        if rect.size.w > 0.0 && rect.size.h > 0.0 {
            self.0 = Some(self.0.map_or(rect, |previous| previous.merge(rect)));
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Layer {
    pub surface: WlSurface,
    pub origin: Point<i32, Logical>,
    pub geometry: SurfaceGeometry,
}

/// Retained RGB pixels, geometry, and pending damage for one pane.
#[derive(Debug, Default)]
pub struct Scene {
    pub(crate) pixels: Vec<u8>,
    pub(crate) layers: Vec<Layer>,
    pub(crate) basis: Option<(FrameSize, f64)>,
    pub(crate) damage: HashMap<WlSurface, SurfaceDamage>,
}

impl Scene {
    /// Read the complete RGB image after the renderer has applied pending
    /// damage.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Forget the retained basis after rebinding or a structural change.
    pub fn invalidate(&mut self) {
        self.basis = None;
        self.damage.clear();
    }

    /// Merge a commit while retaining a bounded rectangle per surface.
    pub fn damage(&mut self, surface: &WlSurface, damage: SurfaceDamage) {
        if self.basis.is_some()
            && let Some(rect) = damage.0
        {
            self.damage.entry(surface.clone()).or_default().add(rect);
        }
    }

    pub(crate) fn damaged(&self, layer: &Layer) -> Option<Rectangle<f64, Logical>> {
        self.damage.get(&layer.surface)?.0
    }
}

#[cfg(test)]
mod tests {
    use smithay::{
        utils::{Buffer, Rectangle, Size, Transform},
        wayland::viewporter::ViewportCachedState,
    };

    use super::*;
    use crate::snapshot::{Opacity, Snapshot};

    fn snapshot(width: u32, height: u32) -> Snapshot {
        let len = usize::try_from(width)
            .unwrap()
            .checked_mul(usize::try_from(height).unwrap())
            .unwrap()
            .checked_mul(4)
            .unwrap();
        Snapshot::from_rgba(width, height, vec![0; len], Opacity::Source)
    }

    fn geometry(snapshot: &Snapshot, scale: i32, viewport: ViewportCachedState) -> SurfaceGeometry {
        SurfaceGeometry::new(snapshot, scale, Transform::Normal, viewport)
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    fn expected(x: f64, y: f64, width: f64, height: f64) -> Rectangle<f64, Logical> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    fn buffer_rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Buffer> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    #[test]
    fn surface_damage_is_clipped_and_merged() {
        let snapshot = snapshot(10, 6);
        let geometry = geometry(&snapshot, 1, ViewportCachedState::default());
        let damage = SurfaceDamage::new(
            &[
                Damage::Surface(rect(1, 1, 2, 2)),
                Damage::Surface(rect(8, 5, 10, 10)),
            ],
            &geometry,
        );

        assert_eq!(damage.0, Some(expected(1.0, 1.0, 9.0, 5.0)));
    }

    #[test]
    fn empty_and_outside_damage_is_ignored() {
        let snapshot = snapshot(10, 6);
        let geometry = geometry(&snapshot, 1, ViewportCachedState::default());
        let damage = SurfaceDamage::new(
            &[
                Damage::Surface(rect(20, 20, 2, 2)),
                Damage::Surface(rect(0, 0, 0, 5)),
            ],
            &geometry,
        );

        assert_eq!(damage.0, None);
    }

    #[test]
    fn buffer_damage_maps_through_a_viewport() {
        let snapshot = snapshot(8, 4);
        let viewport = ViewportCachedState {
            src: Some(Rectangle::new((2.0, 1.0).into(), (4.0, 2.0).into())),
            dst: Some(Size::from((8, 4))),
        };
        let geometry = geometry(&snapshot, 1, viewport);
        let damage = SurfaceDamage::new(&[Damage::Buffer(buffer_rect(3, 1, 1, 1))], &geometry);

        assert_eq!(
            damage.0,
            Some(Rectangle::new((2.0, 0.0).into(), (2.0, 2.0).into()))
        );
    }

    #[test]
    fn buffer_damage_is_clipped_to_the_viewport_source() {
        let snapshot = snapshot(8, 4);
        let viewport = ViewportCachedState {
            src: Some(Rectangle::new((2.0, 1.0).into(), (4.0, 2.0).into())),
            dst: Some(Size::from((8, 4))),
        };
        let geometry = geometry(&snapshot, 1, viewport);
        let damage = SurfaceDamage::new(
            &[
                Damage::Buffer(buffer_rect(1, 1, 2, 1)),
                Damage::Buffer(buffer_rect(6, 1, 1, 1)),
            ],
            &geometry,
        );

        assert_eq!(
            damage.0,
            Some(Rectangle::new((0.0, 0.0).into(), (2.0, 2.0).into()))
        );
    }

    #[test]
    fn buffer_damage_scales_before_mapping_to_the_surface() {
        let snapshot = snapshot(8, 8);
        let geometry = geometry(&snapshot, 2, ViewportCachedState::default());
        let damage = SurfaceDamage::new(&[Damage::Buffer(buffer_rect(1, 1, 2, 2))], &geometry);

        assert_eq!(
            damage.0,
            Some(Rectangle::new((0.5, 0.5).into(), (1.0, 1.0).into()))
        );
    }
}
