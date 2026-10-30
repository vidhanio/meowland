//! Entry points exposing the private terminal decoders to fuzz targets.

use std::io;

use vtparse::CsiParam;

use super::Event;

/// Streaming decoder that [`super::Reader`] feeds one byte at a time.
pub struct Parser(super::Parser);

impl Default for Parser {
    fn default() -> Self {
        Self(super::Parser::new())
    }
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("pending", &self.0.decoder.events.len())
            .finish_non_exhaustive()
    }
}

impl Parser {
    /// Decode one input byte.
    ///
    /// # Errors
    /// Returns an error once an unterminated control sequence exceeds the
    /// recovery limit.
    pub fn push(&mut self, byte: u8) -> io::Result<()> {
        Ok(self.0.push(byte)?)
    }

    /// Pop the oldest decoded event.
    pub fn next_event(&mut self) -> Option<Event> {
        self.0.decoder.events.pop_front()
    }
}

/// Decode a dispatched CSI sequence as a key or focus event.
#[must_use]
pub fn csi(params: &[CsiParam], final_byte: u8) -> Option<Event> {
    super::csi(params, final_byte)
}

/// Decode a complete SGR mouse report, including its `ESC [ <` prefix.
#[must_use]
pub fn mouse(report: &[u8]) -> Option<Event> {
    super::mouse(report)
}
