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

use super::{ConnectionId, Incoming, IncomingSender, Server};

#[derive(Clone, Copy)]
pub(crate) enum ConnectionKind {
    Control,
    Pane,
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
    let (sender, messages) = mpsc::unbounded();
    let (release, reason) = oneshot::channel();
    let output = PaneTransport {
        sender: Some(sender),
        release: Some(release),
        bytes: Rc::new(Cell::new(0)),
    };
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
        result = pane_writer(&mut writer, messages, reason).fuse() => result,
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

impl PaneTransport {
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

async fn pane_writer(
    writer: &mut (impl AsyncWrite + Unpin),
    mut messages: mpsc::UnboundedReceiver<Queued>,
    reason: oneshot::Receiver<String>,
) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    while let Some(Queued {
        message,
        _reservation,
    }) = messages.next().await
    {
        outgoing.send_pane(message)?;
        outgoing.flush_to(writer).await?;
    }
    if let Ok(reason) = reason.await {
        outgoing.send(&ServerToPane::Release(reason))?;
        outgoing.flush_to(writer).await?;
    }
    Ok(())
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), message: &impl Serialize) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    outgoing.send(message)?;
    outgoing.flush_to(writer).await
}
