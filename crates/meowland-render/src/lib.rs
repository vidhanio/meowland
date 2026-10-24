//! Bounded surface snapshots, native-fence DMA-BUF readback, and CPU
//! composition.

mod dmabuf;
mod geometry;
mod render;
mod snapshot;

pub use dmabuf::{DmabufBackend, Readback, SnapshotError, Submission};
pub use geometry::{SurfaceGeometry, source_bound, surface_geometry};
pub use render::{Renderer, extents, popup_origin, surface_stack, viewport_of, window_geometry};
pub use snapshot::Snapshot;
