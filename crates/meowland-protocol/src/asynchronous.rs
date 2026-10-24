//! Cancellation-safe framing and bounded outgoing bytes for async transports.

use std::io::{self, Write};

use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{MAX_MESSAGE, ServerToPane, protocol::decode};

const HEADER: usize = 4;
const IO_BUDGET: usize = 64 * 1024;

/// A pane receiver whose partial message survives cancellation of its future.
pub trait Receiver {
    /// Read the next message without losing partial packets on cancellation.
    ///
    /// # Errors
    /// Returns an error on disconnection, malformed data, or transport failure.
    fn receive(&mut self) -> impl Future<Output = io::Result<ServerToPane>>;
}

/// A bounded packet reader retaining progress between readiness turns.
#[derive(Debug)]
pub struct Reader<R> {
    stream: R,
    header: [u8; HEADER],
    header_read: usize,
    body: Vec<u8>,
    body_size: usize,
    body_read: usize,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub const fn new(stream: R) -> Self {
        Self {
            stream,
            header: [0; HEADER],
            header_read: 0,
            body: Vec::new(),
            body_size: 0,
            body_read: 0,
        }
    }

    /// Read one packet, yielding between bounded chunks.
    /// Cancellation retains all partial header and body bytes.
    ///
    /// # Errors
    /// Returns an error on EOF, invalid packets, or transport failure.
    pub async fn read<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        loop {
            if let Some(message) = self.read_chunk().await? {
                return Ok(message);
            }
            tokio::task::yield_now().await;
        }
    }

    async fn read_chunk<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        if self.header_read < HEADER {
            let read = self
                .stream
                .read(&mut self.header[self.header_read..])
                .await?;
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.header_read += read;
            if self.header_read < HEADER {
                return Ok(None);
            }
            self.body_size = u32::from_le_bytes(self.header) as usize;
            if self.body_size > MAX_MESSAGE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "message too large",
                ));
            }
            self.body.clear();
        }
        if self.body_read < self.body_size {
            let end = (self.body_read + IO_BUDGET).min(self.body_size);
            self.body.resize(end, 0);
            let read = self
                .stream
                .read(&mut self.body[self.body_read..end])
                .await?;
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.body_read += read;
            if self.body_read < self.body_size {
                return Ok(None);
            }
        }
        self.header_read = 0;
        self.body_read = 0;
        decode(&self.body).map(Some)
    }
}

impl<R: AsyncRead + Unpin> Receiver for Reader<R> {
    async fn receive(&mut self) -> io::Result<ServerToPane> {
        self.read().await
    }
}

/// At most one maximum-sized packet's worth of queued bytes.
///
/// Synchronous serialization appends complete packets; async writes retain
/// their offsets, even when another event cancels the current write future.
#[derive(Debug, Default)]
pub struct Outgoing {
    bytes: Vec<u8>,
    offset: usize,
}

impl Outgoing {
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.offset < self.bytes.len()
    }

    /// Write at most 64 KiB, retaining the unsent tail on cancellation.
    ///
    /// # Errors
    /// Returns an error if the transport fails or accepts a zero-byte write.
    pub async fn write_ready(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let end = (self.offset + IO_BUDGET).min(self.bytes.len());
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

    /// Drain queued packets. Cancellation retains all unsent bytes.
    ///
    /// # Errors
    /// Returns an error if the transport fails.
    pub async fn flush_to(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        while self.pending() {
            self.write_ready(writer).await?;
        }
        writer.flush().await
    }
}

impl Write for Outgoing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_MESSAGE + HEADER).saturating_sub(self.bytes.len() - self.offset) {
            return Err(io::Error::other(
                "outgoing pane queue exceeded its byte limit",
            ));
        }
        if self.offset != 0 {
            self.bytes.drain(..self.offset);
            self.offset = 0;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
