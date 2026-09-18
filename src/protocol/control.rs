//! The control socket, and what the command line says to a running server.
//!
//! A request is one connection: it is written, the write end is shut down, and
//! the answer is read back. A connection that says nothing asks whether a
//! server is there, and the connection itself is the answer.

use std::{
    env,
    ffi::OsString,
    io::{Read as _, Write as _},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use smithay::{reexports::wayland_server::BindError, wayland::socket::ListeningSocketSource};

use crate::{
    Error,
    protocol::{WindowId, decode, encode},
};

pub const CONTROL_SOCKET: &str = "meowland-control";
pub const DISPLAY_SOCKET: &str = "meowland-display";

/// The path of a socket in `$XDG_RUNTIME_DIR`, for the side that connects to
/// one.
pub fn path(name: &str) -> Result<PathBuf, Error> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDirectory)?;
    Ok(PathBuf::from(runtime).join(name))
}

/// Listen on a socket in `$XDG_RUNTIME_DIR`.
///
/// The socket file, the lock beside it, and the removal of a socket a dead
/// server left behind are `ListeningSocket`'s: a server that is still alive
/// holds the lock, so a second one is turned away rather than half-bound.
pub fn listen(name: &str) -> Result<ListeningSocketSource, Error> {
    ListeningSocketSource::with_name(name).map_err(|error| match error {
        BindError::AlreadyInUse => Error::AlreadyRunning,
        BindError::RuntimeDirNotSet => Error::NoRuntimeDirectory,
        other => Error::Socket(other),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    List,
    /// Run a program as a client. Its arguments are what a shell gave, which is
    /// not necessarily text, so they cross as the OS strings they are.
    Run(Vec<OsString>),
    Stop,
}

impl Command {
    pub fn encode(&self) -> std::io::Result<Vec<u8>> {
        encode(self)
    }

    pub fn decode(request: &[u8]) -> Option<Self> {
        decode(request).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub label: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Windows(Vec<Window>),
    Ok,
    Failed(String),
}

impl Reply {
    pub fn encode(&self) -> std::io::Result<Vec<u8>> {
        encode(self)
    }

    /// What the server said, or a reason why it made no sense.
    pub fn decode(response: &[u8]) -> Self {
        if response.is_empty() {
            return Self::Failed("the server stopped before it answered".to_owned());
        }
        decode(response)
            .unwrap_or_else(|_| Self::Failed("the server sent something unrecognised".to_owned()))
    }
}

pub fn request(command: &Command) -> Result<Reply, Error> {
    let mut stream = connect(CONTROL_SOCKET)?;
    stream.write_all(&command.encode()?)?;
    // EOF marks the end of the request.
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(Reply::decode(&response))
}

pub fn connect(name: &str) -> Result<UnixStream, Error> {
    let path = path(name)?;
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Err(Error::NotRunning)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    use super::{Command, Reply, Window};
    use crate::protocol::WindowId;

    fn round_trip(command: &Command) {
        let request = command.encode().expect("a command encodes");
        assert_eq!(Command::decode(&request).as_ref(), Some(command));
    }

    #[test]
    fn a_run_request_carries_an_argv_unchanged() {
        // Arguments are what a shell gave: spaces, newlines, a NUL, and bytes
        // that are not text at all.
        let argv = [
            "foot",
            "-T",
            "two words",
            "line\nbreak\ttab",
            "a\0null",
            "\u{fffd}",
        ]
        .map(OsString::from)
        .into_iter()
        .chain([OsString::from_vec(vec![0xff, 0xfe])])
        .collect::<Vec<_>>();
        round_trip(&Command::Run(argv));
    }

    #[test]
    fn the_other_requests_survive_a_round_trip() {
        round_trip(&Command::List);
        round_trip(&Command::Stop);
        assert_eq!(Command::decode(b""), None);
        assert_eq!(Command::decode(b"lately"), None);
    }

    #[test]
    fn a_window_list_survives_a_round_trip() {
        let reply = Reply::Windows(vec![
            Window {
                id: WindowId::new(1),
                label: "foot".to_owned(),
                title: "~ /code".to_owned(),
                active: true,
            },
            Window {
                id: WindowId::new(2),
                label: "two\tlines\nhere".to_owned(),
                title: "and\ttabs\nhere".to_owned(),
                active: false,
            },
            Window {
                id: WindowId::new(3),
                label: String::new(),
                title: String::new(),
                active: false,
            },
        ]);
        let encoded = reply.encode().expect("a reply encodes");
        assert_eq!(Reply::decode(&encoded), reply);
    }

    #[test]
    fn no_windows_is_an_answer_and_no_bytes_is_not() {
        let reply = Reply::Windows(Vec::new());
        assert_eq!(Reply::decode(&reply.encode().unwrap()), reply);
        assert_eq!(
            Reply::decode(b""),
            Reply::Failed("the server stopped before it answered".to_owned())
        );
        assert_eq!(
            Reply::decode(b"windows 2\n"),
            Reply::Failed("the server sent something unrecognised".to_owned())
        );
    }

    #[test]
    fn a_refusal_and_an_acceptance_survive_a_round_trip() {
        for reply in [Reply::Ok, Reply::Failed("no window has that ID".to_owned())] {
            let encoded = reply.encode().expect("a reply encodes");
            assert_eq!(Reply::decode(&encoded), reply);
        }
    }
}
