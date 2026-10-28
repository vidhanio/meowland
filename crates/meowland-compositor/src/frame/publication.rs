//! Sequential whole-frame publication while retained rows are recomposed.

use std::{
    fs::File,
    io::Write as _,
    ops::Range,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameTransport, SharedObject};

/// Publish whole frames through SHM when changed rows cover at least a quarter.
const SHARED_BAND_DIVISOR: usize = 4;

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

    pub(super) fn rows(&mut self, changed: Option<&Range<usize>>, ready: &[u8]) {
        if self.transport != FrameTransport::Shared {
            return;
        }
        if self.object.is_none()
            && !self.force
            && !changed.is_some_and(|rows| {
                (rows.end - rows.start) * SHARED_BAND_DIVISOR
                    >= usize::try_from(self.size.height()).expect("validated height fits usize")
            })
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
            self.rows(None, pixels);
        }
        self.object.take().map(|(object, _)| object)
    }
}
