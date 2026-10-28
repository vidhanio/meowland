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
