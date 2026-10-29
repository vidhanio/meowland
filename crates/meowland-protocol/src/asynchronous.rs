//! Cancellation-safe framing and bounded outgoing bytes for async transports.

use std::{collections::VecDeque, io};

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    MAX_MESSAGE, PanePacket, ServerToPane,
    protocol::{HEADER, decode, message_size, release_oversized},
};
const IO_BUDGET: usize = 64 * 1024;

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
    /// Own a stream and retain partial packet bytes across cancelled reads.
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
            meowland_async::future::yield_now().await;
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
            self.body_size = message_size(self.header)?;
        }
        if self.body_read < self.body_size {
            let end = (self.body_read + IO_BUDGET).min(self.body_size);
            if self.body.len() < end {
                self.body.resize(end, 0);
            }
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
        let decoded = decode(&self.body[..self.body_size]);
        release_oversized(&mut self.body);
        decoded.map(Some)
    }
}

/// At most one maximum-sized packet's worth of queued bytes.
///
/// Frames retain their owned RGB payload separately from their small header.
/// Partial writes retain their offsets across cancellation. A fully written
/// chunk is released back to [`RETAINED_BUFFER`](crate::RETAINED_BUFFER) and
/// reused for the next packet, so the queue never pins an oversized buffer.
#[derive(Debug, Default)]
pub struct Outgoing {
    chunks: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    spare: Vec<u8>,
}

impl Outgoing {
    /// Whether any encoded packet bytes remain to be written.
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.queued != 0
    }

    /// Append a complete packet without a thread-local staging buffer.
    /// Serialization errors leave previously queued packets intact.
    ///
    /// # Errors
    /// Returns an error on serialization failure or if the byte limit is
    /// exceeded.
    pub fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        let size = postcard::experimental::serialized_size(message).map_err(invalid)?;
        self.reserve_packet(size)?;
        let packet = self.encode_packet(message, size, size)?;
        self.push(packet);
        Ok(())
    }

    /// Append a pane message, transferring RGB storage after an explicit
    /// header.
    ///
    /// # Errors
    /// Returns an error on serialization failure or if the byte limit is
    /// exceeded.
    pub fn send_pane(&mut self, message: ServerToPane) -> io::Result<()> {
        let ServerToPane::Frame(frame) = message else {
            return self.send(&message);
        };
        let header = PanePacket::Frame(frame.header());
        let prefix = postcard::experimental::serialized_size(&header).map_err(invalid)?;
        let size = prefix + frame.rgb().len();
        self.reserve_packet(size)?;
        let packet = self.encode_packet(&header, prefix, size)?;
        let rgb = frame.into_rgb();
        self.push(packet);
        self.push(rgb);
        Ok(())
    }

    /// Serialize a packet header into the spare buffer left by the last write.
    fn encode_packet<T: Serialize>(
        &mut self,
        message: &T,
        prefix: usize,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let length = u32::try_from(size).map_err(invalid)?;
        let mut bytes = std::mem::take(&mut self.spare);
        bytes.clear();
        bytes.resize(HEADER + prefix, 0);
        bytes[..HEADER].copy_from_slice(&length.to_le_bytes());
        let encoded = postcard::to_slice(message, &mut bytes[HEADER..]).map_err(invalid)?;
        if encoded.len() != prefix {
            return Err(invalid("serialized message size changed"));
        }
        Ok(bytes)
    }

    fn reserve_packet(&self, size: usize) -> io::Result<()> {
        if size > MAX_MESSAGE
            || self
                .queued
                .checked_add(HEADER + size)
                .is_none_or(|queued| queued > HEADER + MAX_MESSAGE)
        {
            return Err(invalid("outgoing message queue exceeded its byte limit"));
        }
        Ok(())
    }

    fn push(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.queued += bytes.len();
            self.chunks.push_back(bytes);
        }
    }

    /// Write at most 64 KiB, retaining the unsent tail on cancellation.
    ///
    /// # Errors
    /// Returns an error if the transport fails or accepts a zero-byte write.
    pub async fn write_ready(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let Some(bytes) = self.chunks.front() else {
            return Ok(());
        };
        let len = bytes.len();
        let end = (self.offset + IO_BUDGET).min(len);
        let written = writer.write(&bytes[self.offset..end]).await?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.offset += written;
        self.queued -= written;
        if self.offset == len {
            self.release_front();
        }
        Ok(())
    }

    /// Reclaim a fully written chunk as the next packet's buffer.
    fn release_front(&mut self) {
        self.offset = 0;
        if let Some(mut chunk) = self.chunks.pop_front() {
            release_oversized(&mut chunk);
            if chunk.capacity() > self.spare.capacity() {
                self.spare = chunk;
            }
        }
    }

    /// Drain queued packets. Cancellation retains all unsent bytes.
    ///
    /// # Errors
    /// Returns an error if the transport fails.
    pub async fn flush_to(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        while self.pending() {
            self.write_ready(writer).await?;
            meowland_async::future::yield_now().await;
        }
        writer.flush().await
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
