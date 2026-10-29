//! Ordered, resumable terminal writes with a bounded per-turn budget.
use std::{fs::File, io};

use futures::io::{AsyncWrite, AsyncWriteExt};
use meowland_protocol::release_oversized;
use rustix::fs::{self, Mode, OFlags};

/// Most bytes written to the terminal in one scheduling turn.
const WRITE_BUDGET: usize = 64 * 1024;

/// A separate open file description leaves stdout's blocking flags untouched,
/// including the descriptor used to restore terminal modes on exit.
pub(crate) fn open() -> io::Result<File> {
    fs::open(
        "/proc/self/fd/1",
        OFlags::WRONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(Into::into)
}

/// Write bytes without waiting for the terminal to accept them.
///
/// Shutdown uses this after a stalled drain, so a terminal that stops reading
/// cannot keep the pane alive. A partial write leaves escape sequences
/// truncated, which is the price of a bounded exit.
pub(crate) fn write_best_effort(bytes: &[u8]) {
    let Ok(file) = open() else {
        return;
    };
    let mut written = 0;
    while written < bytes.len() {
        match rustix::io::write(&file, &bytes[written..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => written += count,
        }
    }
}

#[derive(Default)]
pub(crate) struct Output {
    bytes: Vec<u8>,
    offset: usize,
}
impl Output {
    pub(crate) const fn pending(&self) -> bool {
        self.offset < self.bytes.len()
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(crate) const fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    pub(crate) const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Drop every queued byte instead of writing it.
    pub(crate) fn discard(&mut self) {
        self.bytes.clear();
        self.offset = 0;
        release_oversized(&mut self.bytes);
    }

    /// Never monopolize the input loop, even when the terminal is writable.
    pub(crate) async fn drain(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let end = self.bytes.len().min(self.offset + WRITE_BUDGET);
        if self.offset < end {
            let written = writer.write(&self.bytes[self.offset..end]).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.offset += written;
        }
        if !self.pending() {
            self.bytes.clear();
            self.offset = 0;
            release_oversized(&mut self.bytes);
        }
        Ok(())
    }
}
