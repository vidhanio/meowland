//! Sequential whole-frame publication while retained rows are recomposed.

use std::{
    fs::File,
    io::Write as _,
    mem,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameTransport, SharedObject};
use meowland_render::Changed;

use super::transfer_columns;

/// Publish whole frames through SHM when the changed rectangle covers at least
/// this fraction of the frame area.
///
/// A shared frame transfers every pixel once; an inline rectangle transfers
/// only the changed columns for the changed rows. Below a quarter of the frame
/// the rectangle is smaller, even after zlib, and skips the publication write.
const SHARED_AREA_DIVISOR: u64 = 4;

/// How far one frame's shared publication has progressed.
enum Stage {
    /// The frame goes out as RGB bands.
    Inline,
    /// Shared transport, but nothing published yet; publication starts once
    /// a changed rectangle is worth a whole frame, or at once for a frame the
    /// pane has no basis for.
    Waiting { whole: bool },
    /// Composed rows stream into the object as they become ready.
    Streaming {
        object: SharedObject,
        file: File,
        written: usize,
    },
}

pub(super) struct Publication {
    size: FrameSize,
    stage: Stage,
    pub elapsed: Duration,
}

impl Publication {
    pub(super) fn new(
        size: FrameSize,
        transport: FrameTransport,
        basis: Option<FrameSize>,
    ) -> Self {
        let stage = match transport {
            FrameTransport::Inline => Stage::Inline,
            FrameTransport::Shared => Stage::Waiting {
                whole: basis != Some(size),
            },
        };
        Self {
            size,
            stage,
            elapsed: Duration::ZERO,
        }
    }

    /// Transport the frame uses, which falls back to inline once a
    /// publication write fails.
    pub(super) const fn transport(&self) -> FrameTransport {
        match self.stage {
            Stage::Inline => FrameTransport::Inline,
            Stage::Waiting { .. } | Stage::Streaming { .. } => FrameTransport::Shared,
        }
    }

    /// Publish the composed prefix `ready` if this frame goes out shared.
    pub(super) fn changed(&mut self, changed: Option<&Changed>, ready: &[u8]) {
        let start = match self.stage {
            Stage::Inline => false,
            Stage::Waiting { whole } => {
                whole || changed.is_some_and(|changed| self.prefers_shared(changed))
            }
            Stage::Streaming { .. } => true,
        };
        if start {
            let started = Instant::now();
            self.write(ready);
            self.elapsed += started.elapsed();
        }
    }

    /// Whether a whole shared frame is cheaper than one inline rectangle.
    fn prefers_shared(&self, changed: &Changed) -> bool {
        let columns = transfer_columns(changed, self.size);
        let width = u64::from(columns.end - columns.start);
        let rows = u64::from(changed.rows.end - changed.rows.start);
        width * rows * SHARED_AREA_DIVISOR
            >= u64::from(self.size.width()) * u64::from(self.size.height())
    }

    fn write(&mut self, ready: &[u8]) {
        let result = match mem::replace(&mut self.stage, Stage::Inline) {
            Stage::Inline => return,
            Stage::Waiting { .. } => {
                SharedObject::create(self.size).map(|(object, file)| (object, file, 0))
            }
            Stage::Streaming {
                object,
                file,
                written,
            } => Ok((object, file, written)),
        }
        .and_then(|(object, mut file, written)| {
            let pending = ready.get(written..).expect("composed prefixes only grow");
            file.write_all(pending)?;
            Ok((object, file, ready.len()))
        });
        match result {
            Ok((object, file, written)) => {
                self.stage = Stage::Streaming {
                    object,
                    file,
                    written,
                };
            }
            Err(error) => {
                tracing::warn!(%error, "shared-memory publication failed; using RGB bands");
            }
        }
    }

    /// Publish the rest of a streamed frame and hand over its object.
    pub(super) fn finish(&mut self, pixels: &[u8]) -> Option<SharedObject> {
        if matches!(self.stage, Stage::Streaming { .. }) {
            self.write(pixels);
        }
        match mem::replace(&mut self.stage, Stage::Inline) {
            Stage::Streaming { object, .. } => {
                self.stage = Stage::Waiting { whole: false };
                Some(object)
            }
            stage => {
                self.stage = stage;
                None
            }
        }
    }
}
