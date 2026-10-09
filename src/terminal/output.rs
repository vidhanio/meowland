//! Ordered, resumable terminal writes with a bounded per-turn budget.
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
};

/// A separate open file description leaves stdout's blocking flags untouched,
/// including the descriptor used to restore terminal modes on exit.
pub(super) fn open() -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open("/proc/self/fd/1")
}

#[derive(Default)]
pub(super) struct Output {
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

    /// Encoding is permitted only after the previous update has drained.
    pub(super) fn buffer(&mut self) -> &mut Vec<u8> {
        assert!(!self.pending());
        &mut self.bytes
    }

    pub(super) const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Never monopolize the input loop, even when the terminal is writable.
    pub(super) fn drain(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let end = self.bytes.len().min(self.offset + 64 * 1024);
        while self.offset < end {
            match writer.write(&self.bytes[self.offset..end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => self.offset += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if !self.pending() {
            self.bytes.clear();
            self.offset = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_writes_resume_without_repeating_bytes() {
        struct Slow {
            bytes: Vec<u8>,
            blocked: bool,
        }
        impl Write for Slow {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.blocked {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.blocked = true;
                let n = bytes.len().min(3);
                self.bytes.extend_from_slice(&bytes[..n]);
                Ok(n)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Output::default();
        let mut writer = Slow {
            bytes: Vec::new(),
            blocked: false,
        };
        output.append(b"frame");
        output.drain(&mut writer).unwrap();
        assert!(output.pending());
        output.append(b"title");
        while output.pending() {
            writer.blocked = false;
            output.drain(&mut writer).unwrap();
        }
        assert_eq!(writer.bytes, b"frametitle");
    }
}
