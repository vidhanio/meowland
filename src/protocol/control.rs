//! The control socket, and what the command line says to a running server.
//!
//! A request is one connection: it is written, the write end is shut down, and
//! the answer is read back. A connection that says nothing asks whether a
//! server is there, and the connection itself is the answer.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{Read as _, Write as _},
    os::unix::{
        ffi::OsStrExt as _,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    Error,
    protocol::{WindowId, decode, encode},
};

pub const CONTROL_SOCKET: &str = "meowland-control";
pub const DISPLAY_SOCKET: &str = "meowland-display";

/// The server's end of a socket. Dropping this removes the socket file.
#[derive(Debug)]
pub struct Socket {
    path: PathBuf,
}

impl Socket {
    pub fn bind(name: &str) -> Result<(Self, UnixListener), Error> {
        let path = Self::path(name)?;
        match UnixStream::connect(&path) {
            Ok(_) => return Err(Error::AlreadyRunning),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)?;
        let socket = Self { path };
        listener.set_nonblocking(true)?;
        Ok((socket, listener))
    }

    fn path(name: &str) -> Result<PathBuf, Error> {
        let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDirectory)?;
        Ok(PathBuf::from(runtime).join(name))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    List,
    Run(Vec<Argument>),
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

/// One argument of a command line: the bytes a shell gave, which are not
/// necessarily text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Argument(Vec<u8>);

impl Argument {
    /// The argument as a program's own argv takes it.
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(&self.0)
    }

    /// The argument as text, for a message to a person.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl From<&OsString> for Argument {
    fn from(argument: &OsString) -> Self {
        Self(argument.as_bytes().to_vec())
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
    let path = Socket::path(name)?;
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

    use super::{Argument, Command, Reply, Window};
    use crate::protocol::WindowId;

    fn round_trip(command: &Command) {
        let request = command.encode().expect("a command encodes");
        assert_eq!(Command::decode(&request).as_ref(), Some(command));
    }

    #[test]
    fn a_run_request_carries_an_argv_unchanged() {
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
        .map(|argument| Argument::from(&argument))
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
