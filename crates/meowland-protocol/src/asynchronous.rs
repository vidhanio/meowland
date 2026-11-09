//! Cancellation-safe framing and bounded outgoing bytes for async transports.

use std::{collections::VecDeque, io};

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    MAX_MESSAGE, PanePacket, ServerToPane,
    protocol::{HEADER, decode, message_size, release_oversized},
};
const IO_BUDGET: usize = 64 * 1024;

/// Where the reader is within the next packet.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// `read` bytes of the length header have arrived.
    Header { read: usize },
    /// `read` of the `size` body bytes have arrived.
    Body { size: usize, read: usize },
    /// A read failed; the stream position is no longer known.
    Failed,
}

/// A bounded packet reader retaining progress between readiness turns.
#[derive(Debug)]
pub struct Reader<R> {
    stream: R,
    header: [u8; HEADER],
    body: Vec<u8>,
    phase: Phase,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    /// Own a stream and retain partial packet bytes across cancelled reads.
    pub const fn new(stream: R) -> Self {
        Self {
            stream,
            header: [0; HEADER],
            body: Vec::new(),
            phase: Phase::Header { read: 0 },
        }
    }

    /// Read one packet, yielding between bounded chunks.
    /// Cancellation retains all partial header and body bytes.
    ///
    /// # Errors
    /// Returns an error on EOF, invalid packets, or transport failure, and on
    /// every read after such an error.
    pub async fn read<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        loop {
            let chunk = self.read_chunk().await;
            if chunk.is_err() {
                self.phase = Phase::Failed;
            }
            if let Some(message) = chunk? {
                return Ok(message);
            }
            meowland_async::future::yield_now().await;
        }
    }

    async fn read_chunk<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        if let Phase::Header { read } = self.phase {
            let count = self.stream.read(&mut self.header[read..]).await?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let read = read + count;
            if read < HEADER {
                self.phase = Phase::Header { read };
                return Ok(None);
            }
            self.phase = Phase::Body {
                size: message_size(self.header)?,
                read: 0,
            };
        }
        let Phase::Body { size, mut read } = self.phase else {
            return Err(io::Error::other("a previous read failed"));
        };
        if read < size {
            let end = (read + IO_BUDGET).min(size);
            if self.body.len() < end {
                self.body.resize(end, 0);
            }
            let count = self.stream.read(&mut self.body[read..end]).await?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            read += count;
            if read < size {
                self.phase = Phase::Body { size, read };
                return Ok(None);
            }
        }
        self.phase = Phase::Header { read: 0 };
        let decoded = decode(&self.body[..size]);
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
    /// Bytes of the front chunk already written.
    offset: usize,
    spare: Vec<u8>,
}

impl Outgoing {
    /// Whether any encoded packet bytes remain to be written.
    #[must_use]
    pub fn pending(&self) -> bool {
        !self.chunks.is_empty()
    }

    /// Encoded bytes not yet written.
    fn queued(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum::<usize>() - self.offset
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
                .queued()
                .checked_add(HEADER + size)
                .is_none_or(|queued| queued > HEADER + MAX_MESSAGE)
        {
            return Err(invalid("outgoing message queue exceeded its byte limit"));
        }
        Ok(())
    }

    fn push(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    use futures::io::AsyncRead;

    use super::*;

    enum Step {
        Bytes(Vec<u8>),
        Pending,
    }

    struct Scripted {
        steps: VecDeque<Step>,
    }

    impl Scripted {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: VecDeque::from(steps),
            }
        }
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            match self.steps.front_mut() {
                Some(Step::Pending) => {
                    self.steps.pop_front();
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Some(Step::Bytes(bytes)) => {
                    let read = bytes.len().min(buffer.len());
                    buffer[..read].copy_from_slice(&bytes[..read]);
                    bytes.drain(..read);
                    if bytes.is_empty() {
                        self.steps.pop_front();
                    }
                    Poll::Ready(Ok(read))
                }
                None => Poll::Ready(Ok(0)),
            }
        }
    }

    fn poll_once<T>(future: &mut Pin<Box<impl std::future::Future<Output = T>>>) -> Poll<T> {
        let mut context = Context::from_waker(Waker::noop());
        future.as_mut().poll(&mut context)
    }

    #[test]
    fn reader_resumes_after_cancellation_mid_header_and_mid_body() {
        let message = "hello".to_owned();
        let body = postcard::to_allocvec(&message).expect("test message encodes");
        let header = u32::try_from(body.len())
            .expect("test body fits the header")
            .to_le_bytes();
        let body_split = body.len() / 2;

        let stream = Scripted::new(vec![
            Step::Bytes(header[..2].to_vec()),
            Step::Pending,
            Step::Bytes(header[2..].to_vec()),
            Step::Pending,
            Step::Bytes(body[..body_split].to_vec()),
            Step::Pending,
            Step::Bytes(body[body_split..].to_vec()),
        ]);
        let mut reader = Reader::new(stream);

        let mut future = Box::pin(reader.read::<String>());
        assert!(poll_once(&mut future).is_pending());
        drop(future);

        let mut future = Box::pin(reader.read::<String>());
        assert!(poll_once(&mut future).is_pending());
        assert!(poll_once(&mut future).is_pending());
        drop(future);

        let mut future = Box::pin(reader.read::<String>());
        assert!(poll_once(&mut future).is_pending());
        drop(future);

        let mut future = Box::pin(reader.read::<String>());
        assert!(poll_once(&mut future).is_pending());
        drop(future);

        let mut future = Box::pin(reader.read::<String>());
        let result = poll_once(&mut future);
        drop(future);
        match result {
            Poll::Ready(Ok(decoded)) => assert_eq!(decoded, message),
            other => panic!("expected the completed message, got {other:?}"),
        }
    }

    #[derive(Default)]
    struct ShortWriter {
        bytes: Vec<u8>,
        pending: bool,
    }

    impl AsyncWrite for ShortWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending = true;
            let count = bytes.len().min(7);
            self.bytes.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        let mut future = Box::pin(future);
        loop {
            if let Poll::Ready(result) = poll_once(&mut future) {
                return result;
            }
        }
    }

    fn drain(outgoing: &mut Outgoing) -> Vec<u8> {
        let mut writer = futures::io::Cursor::new(Vec::new());
        run(outgoing.flush_to(&mut writer)).expect("write packet");
        writer.into_inner()
    }

    #[test]
    fn split_frames_match_normal_framing_and_decode() {
        use meowland_core::pixels::FrameSize;

        for length in [126_usize, 129, 16_383, 16_386] {
            let width = u32::try_from(length / 3).expect("width fits u32");
            let pixels: Vec<u8> = (0..length)
                .map(|index| index.to_le_bytes()[0].wrapping_mul(37))
                .collect();
            for partial in [false, true] {
                let size = if partial {
                    FrameSize::new(width + 1, 2)
                } else {
                    FrameSize::new(width, 1)
                }
                .expect("valid frame size");
                let (x, y) = if partial { (1, 1) } else { (0, 0) };
                let frame = crate::FrameBand::new(size, x, y, width, pixels.clone())
                    .expect("valid frame band");
                let mut ordinary = Outgoing::default();
                ordinary
                    .send(&ServerToPane::Frame(frame.clone()))
                    .expect("encode normal");
                let mut split = Outgoing::default();
                split
                    .send_pane(ServerToPane::Frame(frame))
                    .expect("encode split");
                let bytes = drain(&mut split);
                assert_eq!(
                    bytes,
                    drain(&mut ordinary),
                    "length {length}, partial {partial}"
                );
                let stream = futures::io::Cursor::new(bytes);
                let decoded =
                    run(Reader::new(stream).read::<ServerToPane>()).expect("decode frame");
                let ServerToPane::Frame(decoded) = decoded else {
                    panic!("expected frame");
                };
                assert_eq!(decoded.size(), size);
                assert_eq!((decoded.x(), decoded.y(), decoded.width()), (x, y, width));
                assert_eq!(decoded.rgb(), pixels);
            }
        }
    }

    #[test]
    fn short_writes_resume_after_cancellation_and_preserve_accounting() {
        use meowland_core::pixels::FrameSize;

        let size = FrameSize::new(20, 2).expect("valid size");
        let frame = crate::FrameBand::new(size, 0, 0, 20, (0..120).collect()).expect("valid frame");
        let mut expected = Outgoing::default();
        expected
            .send(&ServerToPane::Frame(frame.clone()))
            .expect("encode frame");
        expected
            .send(&ServerToPane::Title("after frame".into()))
            .expect("encode title");
        let expected = drain(&mut expected);
        let mut outgoing = Outgoing::default();
        outgoing
            .send_pane(ServerToPane::Frame(frame))
            .expect("encode split frame");
        outgoing
            .send_pane(ServerToPane::Title("after frame".into()))
            .expect("encode title");
        let mut writer = ShortWriter::default();

        while outgoing.pending() {
            let mut future = Box::pin(outgoing.write_ready(&mut writer));
            let result = poll_once(&mut future);
            drop(future);
            if result.is_pending() {
                let before = outgoing.queued();
                let mut future = Box::pin(outgoing.write_ready(&mut writer));
                assert!(matches!(poll_once(&mut future), Poll::Ready(Ok(()))));
                drop(future);
                assert!(outgoing.queued() < before);
            } else {
                assert!(matches!(result, Poll::Ready(Ok(()))));
            }
            assert_eq!(outgoing.queued(), expected.len() - writer.bytes.len());
            assert_eq!(writer.bytes, expected[..writer.bytes.len()]);
        }
        assert_eq!(writer.bytes, expected);
    }

    #[test]
    fn rejected_packet_preserves_previously_queued_non_frame_packet() {
        let mut outgoing = Outgoing::default();
        outgoing.send_pane(ServerToPane::Bell).expect("queue bell");
        let queued = outgoing.queued();
        let error = outgoing
            .send_pane(ServerToPane::Title("x".repeat(MAX_MESSAGE - 5)))
            .expect_err("packet exceeding the remaining queue limit must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(outgoing.queued(), queued);
        let bytes = drain(&mut outgoing);
        let decoded = run(Reader::new(futures::io::Cursor::new(bytes)).read::<ServerToPane>())
            .expect("decode queued bell");
        assert!(matches!(decoded, ServerToPane::Bell));
    }
}
