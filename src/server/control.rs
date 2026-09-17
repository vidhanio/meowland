//! The control socket: what a command line asks a running server to do.

use std::{
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};

use crate::{
    Error,
    protocol::control,
    server::{Server, launch, watch},
};

/// How long a control request has to arrive.
///
/// The event loop takes these connections, so one that says nothing gives up
/// rather than hold up the panes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) fn install(
    handle: &LoopHandle<'static, Server>,
    listener: UnixListener,
) -> Result<(), Error> {
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, server: &mut Server| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => server.answer(stream),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|refused| watch("the control socket", refused))?;
    Ok(())
}

impl Server {
    fn answer(&mut self, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let mut request = Vec::new();
        if let Err(error) = stream.read_to_end(&mut request) {
            tracing::warn!(%error, "could not read control request");
            return;
        }
        // A connection that says nothing asks whether a server is there. The
        // connection itself is the answer.
        if request.is_empty() {
            return;
        }
        match self.request(&request).encode() {
            Ok(reply) => {
                if let Err(error) = stream.write_all(&reply)
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    tracing::warn!(%error, "could not answer control request");
                }
            }
            Err(error) => tracing::warn!(%error, "could not encode the answer"),
        }
    }

    fn request(&mut self, request: &[u8]) -> control::Reply {
        match control::Command::decode(request) {
            Some(control::Command::List) => control::Reply::Windows(self.windows.list()),
            Some(control::Command::Run(argv)) => self.run(&argv),
            // The whole server stops, and every client that it started goes
            // with it.
            Some(control::Command::Stop) => {
                tracing::info!("the server was asked to stop");
                self.stop();
                control::Reply::Ok
            }
            None => control::Reply::Failed("unknown request".to_owned()),
        }
    }

    /// Start a client program, and let it draw here.
    fn run(&mut self, argv: &[control::Argument]) -> control::Reply {
        let program = argv.first().map_or_else(
            || "the client".to_owned(),
            control::Argument::to_string_lossy,
        );
        let x_display = self
            .xwayland
            .as_ref()
            .map(crate::server::xwayland::Server::display);
        match launch::start(argv, &self.socket_name, x_display, &self.log) {
            Ok(child) => {
                tracing::info!(program = %program, pid = child.id(), "client started");
                self.children.push(child);
                control::Reply::Ok
            }
            Err(error) => control::Reply::Failed(format!("could not start {program}: {error}")),
        }
    }
}
