//! Ordered, resumable terminal writes with a bounded per-turn budget.
use std::{cell::Cell, fs::File, io, rc::Rc};

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

/// Terminal bytes queued in order, with the stream position of the bytes
/// already written shared with whoever needs to know what reached the
/// terminal.
#[derive(Default)]
pub(crate) struct Output {
    bytes: Vec<u8>,
    offset: usize,
    written: Rc<Cell<usize>>,
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

    /// Stream position of the bytes written so far, updated as writes land.
    pub(crate) fn progress(&self) -> Rc<Cell<usize>> {
        Rc::clone(&self.written)
    }

    /// Stream position just past the last queued byte.
    pub(crate) fn end_position(&self) -> usize {
        self.written.get() + self.bytes.len() - self.offset
    }

    /// Drop every queued byte instead of writing it.
    pub(crate) fn discard(&mut self) {
        self.bytes.clear();
        self.offset = 0;
        release_oversized(&mut self.bytes);
    }

    /// Never monopolize the input loop, even when the terminal is writable.
    #[expect(
        clippy::future_not_send,
        reason = "terminal output belongs to the calloop owner thread"
    )]
    pub(crate) async fn drain(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let end = self.bytes.len().min(self.offset + WRITE_BUDGET);
        if self.offset < end {
            let written = writer.write(&self.bytes[self.offset..end]).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.offset += written;
            self.written.set(self.written.get() + written);
        }
        if !self.pending() {
            self.bytes.clear();
            self.offset = 0;
            release_oversized(&mut self.bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use futures::FutureExt;

    use super::*;

    struct ShortWriter(usize);

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len().min(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stream_positions_survive_partial_writes_and_queue_resets() {
        let mut output = Output::default();
        let progress = output.progress();
        output.append(b"push");
        let push_end = output.end_position();
        output.append(b"restore");
        let mut writer = futures::io::AllowStdIo::new(ShortWriter(push_end - 1));
        output
            .drain(&mut writer)
            .now_or_never()
            .expect("ready")
            .expect("partial write");
        assert_eq!(progress.get(), push_end - 1);
        assert_eq!(output.end_position(), b"pushrestore".len());

        writer.get_mut().0 = usize::MAX;
        output
            .drain(&mut writer)
            .now_or_never()
            .expect("ready")
            .expect("complete write");
        assert!(!output.pending());
        output.append(b"next");
        assert_eq!(
            output.end_position(),
            b"pushrestorenext".len(),
            "positions keep counting after the drained queue is reset"
        );
    }
}
