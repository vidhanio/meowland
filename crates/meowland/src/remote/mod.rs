//! Native SSH presentation using the same async frontend as local panes.

mod ssh;
mod wire;

use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use meowland_core::{Result, asynchronous::Descriptor};
use meowland_protocol::{
    self as protocol, Paths, ServerToPane, Show,
    asynchronous::{Outgoing, Reader, Receiver},
};
use tokio::io::{AsyncRead, AsyncWrite};

pub async fn attach(host: &str, show: Show) -> Result<()> {
    let (session, channel) = tokio::time::timeout(Duration::from_secs(60), ssh::connect(host))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SSH connection timed out"))??;
    let (reader, writer) = tokio::io::split(channel);
    let receiver = RemoteReceiver {
        reader: Reader::new(reader),
        frames: wire::Frames::default(),
    };
    let result = meowland_terminal::attach_connection(receiver, writer, show).await;
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    result
}

struct RemoteReceiver<R> {
    reader: Reader<R>,
    frames: wire::Frames,
}

impl<R: AsyncRead + Unpin> Receiver for RemoteReceiver<R> {
    async fn receive(&mut self) -> io::Result<ServerToPane> {
        self.frames.decode(self.reader.read().await?)
    }
}

pub async fn relay() -> Result<()> {
    if io::stdin().is_terminal() || io::stdout().is_terminal() {
        return Err(io::Error::other("relay requires a binary connection without a PTY").into());
    }
    let server = tokio::net::UnixStream::connect(Paths::discover()?.pane).await?;
    let (server_rx, mut server_tx) = server.into_split();
    let mut input = Descriptor::new(rustix::io::dup(io::stdin()).map_err(io::Error::from)?)?;
    let mut output = Descriptor::new(rustix::io::dup(io::stdout()).map_err(io::Error::from)?)?;
    wire::greet(&mut input, &mut output).await?;
    let incoming = tokio::io::copy(&mut input, &mut server_tx);
    let outgoing = send_frames(Reader::new(server_rx), &mut output);
    tokio::pin!(outgoing);
    tokio::select! {
        result = incoming => match result {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) => outgoing.await,
            Err(error) => Err(error.into()),
        },
        result = &mut outgoing => result,
    }
}

async fn send_frames(
    mut reader: Reader<impl AsyncRead + Unpin>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let mut frames = wire::Frames::default();
    let mut outgoing = Outgoing::default();
    loop {
        let message = reader.read::<ServerToPane>().await?;
        let released = matches!(message, ServerToPane::Release(_) | ServerToPane::Reject(_));
        protocol::send(&mut outgoing, &frames.encode(message)?)?;
        outgoing.flush_to(writer).await?;
        if released {
            return Ok(());
        }
    }
}
