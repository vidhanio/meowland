//! Local socket conversations with ordered, byte-accounted pane output.

use std::{cell::Cell, io, os::unix::net::UnixStream, rc::Rc};

use calloop::LoopHandle;
use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    io::{AsyncReadExt, AsyncWrite},
};
use meowland_async::asynchronous::timeout;
use meowland_panes::PaneId;
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

/// Answer one control request.
///
/// A peer that shuts down its write half after the request still gets the
/// response; any further request bytes abort the conversation.
async fn control(
    mut reader: impl futures::io::AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    mut incoming: IncomingSender,
) -> io::Result<()> {
    let request: ControlRequest = Reader::new(&mut reader).read().await?;
    let (reply, mut answer) = oneshot::channel();
    incoming
        .send(Incoming::Control(request, reply))
        .await
        .map_err(io::Error::other)?;
    let mut extra = [0];
    let response = futures::select_biased! {
        result = reader.read(&mut extra).fuse() => {
            if result? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected bytes after the control request",
                ));
            }
            answer.await
        }
        result = answer => result,
    }
    .map_err(io::Error::other)?;
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

/// Ordered, byte-accounted output to one admitted pane.
pub(crate) struct PaneTransport {
    link: Link,
    bytes: Rc<Cell<usize>>,
}

/// Whether the pane still accepts output.
enum Link {
    Open {
        sender: mpsc::UnboundedSender<Queued>,
        release: oneshot::Sender<String>,
    },
    /// The writer drains queued output and then tells the pane why it was
    /// released.
    Released,
}

/// Outcome of [`PaneTransport::send`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Queued,
    /// The pane was released and accepts no more output.
    Released,
    /// The message exceeds the output budget or the writer is gone.
    Rejected,
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
            link: Link::Open { sender, release },
            bytes: Rc::new(Cell::new(0)),
        };
        (transport, PaneQueue { messages, reason })
    }

    pub(crate) const fn active(&self) -> bool {
        matches!(self.link, Link::Open { .. })
    }

    pub(crate) fn send(&self, message: ServerToPane) -> Sent {
        let Link::Open { sender, .. } = &self.link else {
            return Sent::Released;
        };
        let Ok(size) = postcard::experimental::serialized_size(&message) else {
            return Sent::Rejected;
        };
        let bytes = size + std::mem::size_of::<Queued>();
        let limit = protocol::MAX_MESSAGE + std::mem::size_of::<Queued>();
        let Some(total) = self
            .bytes
            .get()
            .checked_add(bytes)
            .filter(|&total| total <= limit && size <= protocol::MAX_MESSAGE)
        else {
            return Sent::Rejected;
        };
        self.bytes.set(total);
        let queued = Queued {
            message,
            _reservation: Reservation {
                total: Rc::clone(&self.bytes),
                bytes,
            },
        };
        if sender.unbounded_send(queued).is_ok() {
            Sent::Queued
        } else {
            Sent::Rejected
        }
    }

    pub(crate) fn release(&mut self, reason: String) {
        if let Link::Open { release, .. } = std::mem::replace(&mut self.link, Link::Released) {
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
    use meowland_protocol::ControlResponse;

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
        assert_eq!(
            transport.send(ServerToPane::Title("x".repeat(8 << 20))),
            Sent::Queued
        );
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
        assert_eq!(
            transport.send(ServerToPane::Title(title.clone())),
            Sent::Queued
        );
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

    #[test]
    fn extra_control_bytes_take_priority_over_a_ready_reply() {
        use std::io::Write;

        let (socket, mut peer) = UnixStream::pair().expect("socket pair");
        protocol::send(&mut peer, &ControlRequest::List).expect("send request");
        let (incoming, mut requests) = mpsc::channel(1);
        let mut event_loop = EventLoop::<Option<io::Result<()>>>::try_new().expect("loop");
        let handle = event_loop.handle();
        let reader = handle
            .adapt_io(socket.try_clone().expect("clone socket"))
            .expect("adapt reader");
        let writer = handle.adapt_io(socket).expect("adapt writer");
        let (executor, scheduler) = calloop::futures::executor().expect("executor");
        handle
            .insert_source(executor, |result, (), finished| *finished = Some(result))
            .expect("register executor");
        scheduler
            .schedule(control(reader, writer, incoming))
            .expect("schedule control");
        let mut finished = None;
        let started = Instant::now();
        let reply = loop {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "request was not received"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut finished)
                .expect("dispatch");
            assert!(finished.is_none(), "control stopped before its reply");
            if let Ok(Incoming::Control(ControlRequest::List, reply)) = requests.try_recv() {
                break reply;
            }
        };
        peer.write_all(&[42]).expect("send extra byte");
        reply.send(ControlResponse::Ok).expect("send reply");
        while finished.is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "control did not finish"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut finished)
                .expect("dispatch");
        }
        assert_eq!(
            finished
                .expect("control finished")
                .expect_err("extra bytes rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_half_closed_control_request_is_answered() {
        use std::net::Shutdown;

        let (socket, mut peer) = UnixStream::pair().expect("socket pair");
        protocol::send(&mut peer, &ControlRequest::List).expect("send request");
        peer.shutdown(Shutdown::Write).expect("half-close request");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let (incoming, mut requests) = mpsc::channel(1);
        let mut event_loop = EventLoop::<Option<io::Result<()>>>::try_new().expect("loop");
        let handle = event_loop.handle();
        let reader = handle
            .adapt_io(socket.try_clone().expect("clone socket"))
            .expect("adapt reader");
        let writer = handle.adapt_io(socket).expect("adapt writer");
        let (executor, scheduler) = calloop::futures::executor().expect("executor");
        handle
            .insert_source(executor, |result, (), finished| *finished = Some(result))
            .expect("register executor");
        scheduler
            .schedule(control(reader, writer, incoming))
            .expect("schedule control");
        let mut finished = None;
        let started = Instant::now();
        let reply = loop {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "request was not received"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut finished)
                .expect("dispatch");
            assert!(finished.is_none(), "control stopped before its reply");
            if let Ok(request) = requests.try_recv() {
                let Incoming::Control(ControlRequest::List, reply) = request else {
                    panic!("expected a list request");
                };
                break reply;
            }
        };
        event_loop
            .dispatch(Some(Duration::from_millis(20)), &mut finished)
            .expect("dispatch EOF");
        assert!(
            finished.is_none(),
            "half-close must not abort the pending reply"
        );
        reply.send(ControlResponse::Ok).expect("send reply");
        while finished.is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "control did not finish"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut finished)
                .expect("dispatch");
        }
        finished
            .expect("control finished")
            .expect("a half-closed request is answered");
        let response: ControlResponse = protocol::recv(&mut peer).expect("decode response");
        assert!(matches!(response, ControlResponse::Ok), "{response:?}");
    }
}
