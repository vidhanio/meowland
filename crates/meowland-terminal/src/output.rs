//! Ordered, resumable terminal writes with a bounded per-turn budget.
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
};

use tokio::io::{AsyncWrite, AsyncWriteExt};

/// A separate open file description leaves stdout's blocking flags untouched,
/// including the descriptor used to restore terminal modes on exit.
pub fn open() -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags((rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOCTTY).bits() as i32)
        .open("/proc/self/fd/1")
}

#[derive(Default)]
pub struct Output {
    bytes: Vec<u8>,
    offset: usize,
}
impl Output {
    pub(super) const fn pending(&self) -> bool {
        self.offset < self.bytes.len()
    }

    pub(super) fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(super) const fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    pub(super) const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Never monopolize the input loop, even when the terminal is writable.
    pub(super) async fn drain(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let end = self.bytes.len().min(self.offset + 64 * 1024);
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
        }
        Ok(())
    }
}
