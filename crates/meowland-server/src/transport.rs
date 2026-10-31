//! Local socket conversations with ordered, byte-accounted pane output.

use std::{cell::Cell, io, os::unix::net::UnixStream, rc::Rc};

use calloop::LoopHandle;
use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    io::{AsyncReadExt, AsyncWrite},
};
use meowland_async::asynchronous::timeout;
use meowland_compositor::PaneId;
use meowland_protocol::{
    self as protocol, ControlRequest, PaneToServer, ServerToPane,
    asynchronous::{Outgoing, Reader},
};
use serde::Serialize;

use super::{ConnectionId, Incoming, IncomingSender, MAX_CONNECTIONS, Server};

#[derive(Clone, Copy)]
pub(crate) enum ConnectionKind {
    Control,
    Pane,
}

impl ConnectionKind {
    /// Open connections allowed when accepting this kind; panes leave the last
    /// slot for control traffic.
    pub(crate) const fn capacity(self) -> usize {
        match self {
            Self::Control => MAX_CONNECTIONS,
            Self::Pane => MAX_CONNECTIONS - 1,
        }
    }
}

#[expect(
    clippy::future_not_send,
    reason = "socket conversations run on calloop's local executor"
)]
pub(crate) async fn connection(
    socket: UnixStream,
    kind: ConnectionKind,
    id: ConnectionId,
    incoming: IncomingSender,
    handle: LoopHandle<'static, Server>,
) -> io::Result<()> {
    let reader = handle.adapt_io(socket.try_clone()?)?;
    let writer = handle.adapt_io(socket)?;
    match kind {
        ConnectionKind::Control => {
            timeout(
                &handle,
                protocol::REQUEST_TIMEOUT,
                control(reader, writer, incoming),
            )
            .await
        }
        ConnectionKind::Pane => pane(reader, writer, id, incoming, &handle).await,
    }
}

async fn control(
    mut reader: impl futures::io::AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    mut incoming: IncomingSender,
) -> io::Result<()> {
    let request: ControlRequest = Reader::new(&mut reader).read().await?;
    let (reply, answer) = oneshot::channel();
    incoming
        .send(Incoming::Control(request, reply))
        .await
        .map_err(io::Error::other)?;
    let mut eof = [0];
    let response = futures::select! {
        result = answer.fuse() => result.map_err(io::Error::other)?,
        result = reader.read(&mut eof).fuse() => {
            result?;
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
    };
    send(&mut writer, &response).await
}

async fn pane(
    reader: impl futures::io::AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    id: ConnectionId,
    mut incoming: IncomingSender,
    handle: &LoopHandle<'static, Server>,
) -> io::Result<()> {
    let mut reader = Reader::new(reader);
    let Ok(PaneToServer::Hello(hello)) =
        timeout(handle, protocol::REQUEST_TIMEOUT, reader.read()).await
    else {
        let reason = "invalid or timed-out pane hello";
        let _ = timeout(
            handle,
            protocol::REQUEST_TIMEOUT,
            send(&mut writer, &ServerToPane::Reject(reason.into())),
        )
        .await;
        return Err(io::Error::new(io::ErrorKind::InvalidInput, reason));
    };
    let pane = PaneId::from(id);
    let (output, queue) = PaneTransport::new();
    incoming
        .send(Incoming::Focus(pane, hello, output))
        .await
        .map_err(io::Error::other)?;
    let receiving = async {
        loop {
            let PaneToServer::Action(action) = reader.read().await? else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate pane hello",
                ));
            };
            incoming
                .send(Incoming::PaneMessage(pane, action))
                .await
                .map_err(io::Error::other)?;
        }
    };
    futures::select! {
        result = receiving.fuse() => result,
        result = pane_writer(&mut writer, queue, handle).fuse() => result,
    }
}

pub(crate) struct PaneTransport {
    sender: Option<mpsc::UnboundedSender<Queued>>,
    release: Option<oneshot::Sender<String>>,
    bytes: Rc<Cell<usize>>,
}

struct Reservation {
    total: Rc<Cell<usize>>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.total.set(self.total.get() - self.bytes);
    }
}

struct Queued {
    message: ServerToPane,
    _reservation: Reservation,
}

/// The writer's side of a [`PaneTransport`].
struct PaneQueue {
    messages: mpsc::UnboundedReceiver<Queued>,
    reason: oneshot::Receiver<String>,
}

impl PaneTransport {
    fn new() -> (Self, PaneQueue) {
        let (sender, messages) = mpsc::unbounded();
        let (release, reason) = oneshot::channel();
        let transport = Self {
            sender: Some(sender),
            release: Some(release),
            bytes: Rc::new(Cell::new(0)),
        };
        (transport, PaneQueue { messages, reason })
    }

    pub(crate) const fn active(&self) -> bool {
        self.sender.is_some()
    }

    pub(crate) fn send(&self, message: ServerToPane) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        let Ok(size) = postcard::experimental::serialized_size(&message) else {
            return false;
        };
        let bytes = size + std::mem::size_of::<Queued>();
        let limit = protocol::MAX_MESSAGE + std::mem::size_of::<Queued>();
        let Some(total) = self
            .bytes
            .get()
            .checked_add(bytes)
            .filter(|&total| total <= limit && size <= protocol::MAX_MESSAGE)
        else {
            return false;
        };
        self.bytes.set(total);
        sender
            .unbounded_send(Queued {
                message,
                _reservation: Reservation {
                    total: Rc::clone(&self.bytes),
                    bytes,
                },
            })
            .is_ok()
    }

    pub(crate) fn release(&mut self, reason: String) {
        self.sender.take();
        if let Some(release) = self.release.take() {
            let _ = release.send(reason);
        }
    }
}

/// Write queued pane output in order until the pane is released, then drain
/// the rest and the release notice within [`protocol::REQUEST_TIMEOUT`].
///
/// Release is observed even while a message is being flushed, so a peer that
/// stops reading cannot hold its connection past the drain deadline. The
/// notice is only appended after every queued packet is complete.
async fn pane_writer<D: 'static>(
    writer: &mut (impl AsyncWrite + Unpin),
    PaneQueue {
        mut messages,
        mut reason,
    }: PaneQueue,
    handle: &LoopHandle<'static, D>,
) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    let released = loop {
        let queued = futures::select_biased! {
            released = reason => break released,
            queued = messages.next() => queued,
        };
        let Some(Queued {
            message,
            _reservation,
        }) = queued
        else {
            break reason.await;
        };
        outgoing.send_pane(message)?;
        futures::select_biased! {
            released = reason => break released,
            result = outgoing.flush_to(writer).fuse() => result?,
        }
    };
    let Ok(released) = released else {
        return Ok(());
    };
    timeout(handle, protocol::REQUEST_TIMEOUT, async {
        outgoing.flush_to(writer).await?;
        while let Some(Queued { message, .. }) = messages.next().await {
            outgoing.send_pane(message)?;
            outgoing.flush_to(writer).await?;
        }
        outgoing.send(&ServerToPane::Release(released))?;
        outgoing.flush_to(writer).await
    })
    .await
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), message: &impl Serialize) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    outgoing.send(message)?;
    outgoing.flush_to(writer).await
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use calloop::EventLoop;

    use super::*;

    #[test]
    fn a_released_pane_that_stops_reading_is_disconnected() {
        let mut event_loop = EventLoop::<Option<io::Result<()>>>::try_new().expect("event loop");
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().expect("executor");
        handle
            .insert_source(executor, |result, (), finished| *finished = Some(result))
            .expect("register executor");
        let (socket, _peer) = UnixStream::pair().expect("socket pair");
        let mut writer = handle.adapt_io(socket).expect("adapt socket");
        let (mut transport, queue) = PaneTransport::new();
        assert!(transport.send(ServerToPane::Title("x".repeat(8 << 20))));
        let writing = handle.clone();
        scheduler
            .schedule(async move { pane_writer(&mut writer, queue, &writing).await })
            .expect("schedule writer");
        let mut finished = None;
        event_loop
            .dispatch(Some(Duration::ZERO), &mut finished)
            .expect("dispatch");
        assert!(
            finished.is_none(),
            "the writer should be blocked on the peer"
        );

        transport.release("closed".into());
        let started = Instant::now();
        while finished.is_none() {
            assert!(
                started.elapsed() < protocol::REQUEST_TIMEOUT * 2,
                "the released writer outlived its drain deadline"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(50)), &mut finished)
                .expect("dispatch");
        }

        let error = finished
            .expect("writer finished")
            .expect_err("a stalled drain fails");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn release_finishes_a_partial_packet_before_the_notice() {
        use std::io::Read;

        let mut event_loop = EventLoop::<Option<io::Result<()>>>::try_new().expect("event loop");
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().expect("executor");
        handle
            .insert_source(executor, |result, (), finished| *finished = Some(result))
            .expect("register executor");
        let (socket, mut peer) = UnixStream::pair().expect("socket pair");
        peer.set_read_timeout(Some(protocol::REQUEST_TIMEOUT * 2))
            .expect("set read timeout");
        let mut writer = handle.adapt_io(socket).expect("adapt socket");
        let (mut transport, queue) = PaneTransport::new();
        let title = "x".repeat(8 << 20);
        assert!(transport.send(ServerToPane::Title(title.clone())));
        let writing = handle.clone();
        scheduler
            .schedule(async move { pane_writer(&mut writer, queue, &writing).await })
            .expect("schedule writer");
        let mut finished = None;
        event_loop
            .dispatch(Some(Duration::ZERO), &mut finished)
            .expect("dispatch");
        assert!(finished.is_none(), "packet must not have completed");
        let mut prefix = [0];
        peer.read_exact(&mut prefix).expect("packet has begun");
        transport.release("closed".into());
        let reading = std::thread::spawn(move || {
            let mut stream = io::Cursor::new(prefix).chain(peer);
            let packet: ServerToPane = protocol::recv(&mut stream).expect("decode packet");
            let notice: ServerToPane = protocol::recv(&mut stream).expect("decode release");
            (packet, notice)
        });
        let started = Instant::now();
        while finished.is_none() {
            assert!(
                started.elapsed() < protocol::REQUEST_TIMEOUT * 2,
                "writer did not finish draining"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut finished)
                .expect("dispatch");
        }
        finished.expect("writer finished").expect("drain succeeds");
        let (packet, notice) = reading.join().expect("peer thread");
        assert!(matches!(packet, ServerToPane::Title(value) if value == title));
        assert!(matches!(notice, ServerToPane::Release(reason) if reason == "closed"));
    }
}
