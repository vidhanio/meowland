//! Connection tasks and byte-accounted, ordered pane output.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use meowland_compositor::PaneId;
use meowland_core::pixels::FrameSize;
use meowland_protocol::{
    self as protocol, ControlRequest, PaneToServer, ServerToPane,
    asynchronous::{Outgoing, Reader},
};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWrite},
    net::UnixStream,
    sync::{mpsc, oneshot},
};

use super::{Incoming, IncomingSender};

#[derive(Clone, Copy)]
pub(crate) enum ConnectionKind {
    Control,
    Pane,
}

pub(crate) async fn connection(
    socket: UnixStream,
    kind: ConnectionKind,
    id: PaneId,
    incoming: IncomingSender,
) -> io::Result<()> {
    match kind {
        ConnectionKind::Control => {
            tokio::time::timeout(protocol::REQUEST_TIMEOUT, control(socket, incoming)).await?
        }
        ConnectionKind::Pane => pane(socket, id, incoming).await,
    }
}

async fn control(socket: UnixStream, incoming: IncomingSender) -> io::Result<()> {
    let (mut reader, mut writer) = socket.into_split();
    let request: ControlRequest = Reader::new(&mut reader).read().await?;
    let (reply, answer) = oneshot::channel();
    incoming
        .send(Incoming::Control(request, reply))
        .await
        .map_err(io::Error::other)?;
    let mut eof = [0];
    let response = tokio::select! {
        result = answer => result.map_err(io::Error::other)?,
        result = reader.read(&mut eof) => {
            result?;
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
    };
    send(&mut writer, &response).await
}

async fn pane(socket: UnixStream, id: PaneId, incoming: IncomingSender) -> io::Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = Reader::new(reader);
    let hello = match tokio::time::timeout(protocol::REQUEST_TIMEOUT, reader.read()).await {
        Ok(Ok(PaneToServer::Hello(hello)))
            if FrameSize::new(hello.width, hello.height).is_some() =>
        {
            hello
        }
        _ => {
            let reason = "invalid or timed-out pane hello";
            let _ = tokio::time::timeout(
                protocol::REQUEST_TIMEOUT,
                send(&mut writer, &ServerToPane::Reject(reason.into())),
            )
            .await;
            return Err(io::Error::new(io::ErrorKind::InvalidInput, reason));
        }
    };
    let (sender, messages) = mpsc::unbounded_channel();
    let (release, reason) = oneshot::channel();
    let output = PaneTransport {
        sender: Some(sender),
        release: Some(release),
        bytes: Arc::new(AtomicUsize::new(0)),
    };
    incoming
        .send(Incoming::Attach(id, hello, output))
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
                .send(Incoming::PaneMessage(id, action))
                .await
                .map_err(io::Error::other)?;
        }
    };
    tokio::select! {
        result = receiving => result,
        result = pane_writer(&mut writer, messages, reason) => result,
    }
}

pub(crate) struct PaneTransport {
    sender: Option<mpsc::UnboundedSender<Queued>>,
    release: Option<oneshot::Sender<String>>,
    bytes: Arc<AtomicUsize>,
}

struct Reservation {
    total: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
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
        if size > protocol::MAX_MESSAGE
            || self
                .bytes
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                    total.checked_add(bytes).filter(|&total| total <= limit)
                })
                .is_err()
        {
            return false;
        }
        let queued = Queued {
            message,
            _reservation: Reservation {
                total: Arc::clone(&self.bytes),
                bytes,
            },
        };
        sender.send(queued).is_ok()
    }

    pub(crate) fn release(&mut self, reason: String) {
        self.sender.take();
        if let Some(release) = self.release.take() {
            let _ = release.send(reason);
        }
    }
}

async fn pane_writer(
    writer: &mut (impl AsyncWrite + Unpin + Send),
    mut messages: mpsc::UnboundedReceiver<Queued>,
    reason: oneshot::Receiver<String>,
) -> io::Result<()> {
    while let Some(Queued {
        message,
        _reservation,
    }) = messages.recv().await
    {
        let mut outgoing = Outgoing::default();
        outgoing.send_pane(message)?;
        outgoing.flush_to(writer).await?;
    }
    if let Ok(reason) = reason.await {
        send(writer, &ServerToPane::Release(reason)).await?;
    }
    Ok(())
}

async fn send(
    writer: &mut (impl AsyncWrite + Unpin + Send),
    message: &(impl Serialize + Sync),
) -> io::Result<()> {
    let mut outgoing = Outgoing::default();
    outgoing.send(message)?;
    outgoing.flush_to(writer).await
}
