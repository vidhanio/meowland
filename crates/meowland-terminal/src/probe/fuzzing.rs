//! Entry point exposing the private capability-reply parser to fuzz targets.

use std::fmt::Debug;

/// Capability-reply parser fed with read chunks during the handshake.
#[derive(Default)]
pub struct ProbeParser(super::ProbeParser);

impl Debug for ProbeParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeParser")
            .field("info", &self.0.info)
            .finish_non_exhaustive()
    }
}

impl ProbeParser {
    /// Decode one chunk of terminal replies.
    pub fn parse(&mut self, bytes: &[u8]) {
        self.0.parse(bytes);
    }

    /// Whether a device-attributes reply has started the quiet window.
    #[must_use]
    pub const fn attributes_seen(&self) -> bool {
        self.0.attributes_seen()
    }

    /// Discovered capabilities.
    #[must_use]
    pub fn finish(self) -> impl Debug + Eq {
        self.0.finish()
    }
}
