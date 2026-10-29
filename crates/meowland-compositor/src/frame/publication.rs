//! Sequential whole-frame publication while retained rows are recomposed.

use std::{
    fs::File,
    io::Write as _,
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

pub(super) struct Publication {
    size: FrameSize,
    force: bool,
    pub transport: FrameTransport,
    object: Option<(SharedObject, File)>,
    written: usize,
    pub elapsed: Duration,
}

impl Publication {
    pub(super) fn new(
        size: FrameSize,
        transport: FrameTransport,
        basis: Option<FrameSize>,
    ) -> Self {
        Self {
            size,
            force: basis != Some(size),
            transport,
            object: None,
            written: 0,
            elapsed: Duration::ZERO,
        }
    }

    pub(super) fn changed(&mut self, changed: Option<&Changed>, ready: &[u8]) {
        if self.transport != FrameTransport::Shared {
            return;
        }
        if self.object.is_none()
            && !self.force
            && !changed.is_some_and(|changed| self.prefers_shared(changed))
        {
            return;
        }
        let started = Instant::now();
        let result = self.write(ready);
        self.elapsed += started.elapsed();
        if let Err(error) = result {
            tracing::warn!(%error, "shared-memory publication failed; using RGB bands");
            self.object = None;
            self.transport = FrameTransport::Inline;
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

    fn write(&mut self, ready: &[u8]) -> std::io::Result<()> {
        if self.object.is_none() {
            self.object = Some(SharedObject::create(self.size)?);
        }
        if let Some((_, file)) = &mut self.object {
            file.write_all(&ready[self.written..])?;
            self.written = ready.len();
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, pixels: &[u8]) -> Option<SharedObject> {
        if self.object.is_some() {
            self.changed(None, pixels);
        }
        self.object.take().map(|(object, _)| object)
    }
}
