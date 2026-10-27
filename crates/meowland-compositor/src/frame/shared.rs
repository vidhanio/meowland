//! Streaming composition with lazy whole-frame SHM publication and row-band fallback.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::ops::Range;

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, FrameTransport, ServerToPane, SharedObject};
use meowland_render::{Renderer, Snapshot};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{PaneState, Presentation};

/// Keep row bands inline until they cover at least one quarter of the frame.
const SHARED_BAND_DIVISOR: usize = 4;

struct Publication<'a> {
    size: FrameSize,
    shown: &'a mut Vec<u8>,
    valid: bool,
    changed: Option<Range<usize>>,
    object: Option<(SharedObject, File)>,
    transport: FrameTransport,
}

impl<'a> Publication<'a> {
    fn new(state: &'a mut PaneState) -> Self {
        let size = state.size;
        let valid = state.shown_size == Some(size);
        state.shown.resize(size.rgb_len(), 0);
        Self {
            size,
            shown: &mut state.shown,
            valid,
            changed: None,
            object: None,
            transport: FrameTransport::Shared,
        }
    }

    fn rows(&mut self, y: u32, pixels: &[u8]) {
        let stride = self.size.rgb_stride();
        let start = usize::try_from(y).expect("rendered row fits usize") * stride;
        let end = start + pixels.len();
        for (offset, (old, new)) in self.shown[start..end]
            .chunks_exact_mut(stride)
            .zip(pixels.chunks_exact(stride))
            .enumerate()
        {
            if !self.valid || old != new {
                old.copy_from_slice(new);
                let row = start / stride + offset;
                self.changed = Some(
                    self.changed
                        .as_ref()
                        .map_or(row..row + 1, |changed| changed.start..row + 1),
                );
            }
        }
        let result = if let Some((_, file)) = &mut self.object {
            file.write_all(pixels)
        } else if self.transport == FrameTransport::Shared
            && self.changed.as_ref().is_some_and(|rows| {
                !self.valid
                    || (rows.end - rows.start) * SHARED_BAND_DIVISOR
                        >= usize::try_from(self.size.height()).expect("height fits usize")
            })
        {
            SharedObject::create(self.size).and_then(|(object, mut file)| {
                file.write_all(&self.shown[..end])?;
                self.object = Some((object, file));
                Ok(())
            })
        } else {
            Ok(())
        };
        if let Err(error) = result {
            tracing::warn!(%error, "upstream shared-memory publication failed; using RGB bands");
            self.object = None;
            self.transport = FrameTransport::Inline;
        }
    }
}

pub(super) fn render(
    state: &mut PaneState,
    renderer: &mut Renderer,
    root: Option<&WlSurface>,
    snapshots: &HashMap<WlSurface, Snapshot>,
    scale: f64,
) -> Option<ServerToPane> {
    let mut publication = Publication::new(state);
    renderer.render_rows(root, snapshots, publication.size, scale, |y, pixels| {
        publication.rows(y, pixels);
    });
    let Publication {
        size,
        changed,
        object,
        transport,
        ..
    } = publication;
    state.transport = transport;
    state.presentation = Presentation::Clean;
    let rows = changed?;
    state.shown_size = Some(size);
    state.presentation = Presentation::InFlight { dirty: false };
    if let Some((object, _file)) = object {
        let frame = *object.frame();
        state.shared = Some(object);
        Some(ServerToPane::SharedFrame(frame))
    } else {
        let bytes = rows.start * size.rgb_stride()..rows.end * size.rgb_stride();
        Some(ServerToPane::Frame(
            FrameBand::new(
                size,
                u32::try_from(rows.start).expect("row offset fits validated frame height"),
                state.shown[bytes].to_vec(),
            )
            .expect("rendered band fits its frame"),
        ))
    }
}
