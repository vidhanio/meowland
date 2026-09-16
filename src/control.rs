//! The sockets a server is reached on.
//!
//! A server has no terminal of its own, so everything said to it and shown by
//! it goes through `$XDG_RUNTIME_DIR`: a CLI connects to the control socket,
//! sends one command, reads one reply and exits, and a terminal connects to the
//! display socket to be shown the server (`src/display.rs`). Both are bound by
//! the server, and both are named here because this is the only place that
//! knows where they live and what a socket file with nothing behind it means.
//!
//! `$XDG_RUNTIME_DIR` is per-user and reachable only by its owner, which is the
//! whole of the access control - and it has to be, because one of these
//! commands starts a process.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{Read as _, Write as _},
    os::unix::{
        ffi::OsStringExt as _,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

/// The socket a server listens on and a CLI connects to.
pub const CONTROL_SOCKET: &str = "meowland-control";

/// The socket a terminal connects to to be shown the server.
pub const DISPLAY_SOCKET: &str = "meowland-display";

/// What separates the arguments of a `run` request.
///
/// An argument may hold any byte but this one, so an argv crosses the socket
/// unchanged. Quoting would have to be written twice, once to write it and once
/// to read it, and would be wrong about something either way.
const ARGUMENT_SEPARATOR: u8 = 0;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDirectory,
    #[error("another meowland server is already running")]
    AlreadyRunning,
    #[error("no meowland server is running")]
    NotRunning,
    #[error("the control socket could not be used")]
    Io(#[from] std::io::Error),
}

/// The server's end of the socket: the file is removed when this is dropped.
#[derive(Debug)]
pub struct Socket {
    path: PathBuf,
}

impl Socket {
    /// Take one of the server's socket names, or report that a server
    /// already holds it.
    pub fn bind(name: &str) -> Result<(Self, UnixListener), Error> {
        let path = path(name)?;
        match UnixStream::connect(&path) {
            // Something answered: a server is running, and it is not this one.
            Ok(_) => return Err(Error::AlreadyRunning),
            // A socket file with nothing behind it is what a killed server
            // leaves, and taking the name over is what makes it recoverable.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok((Self { path }, listener))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// One command a CLI sends a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Every window the server has.
    List,
    /// Start a client as another window of the server.
    Run(Vec<OsString>),
    /// Stop the server and everything started under it.
    Quit,
}

impl Command {
    /// The request to send for this command.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::List => b"list".to_vec(),
            Self::Quit => b"quit".to_vec(),
            Self::Run(argv) => {
                let mut request = b"run".to_vec();
                for argument in argv {
                    request.push(ARGUMENT_SEPARATOR);
                    request.extend_from_slice(argument.as_encoded_bytes());
                }
                request
            }
        }
    }

    /// Read a request, or `None` if it names no command this server has.
    pub fn decode(request: &[u8]) -> Option<Self> {
        if request == b"list" {
            return Some(Self::List);
        }
        if request == b"quit" {
            return Some(Self::Quit);
        }
        let mut fields = request.split(|byte| *byte == ARGUMENT_SEPARATOR);
        if fields.next() != Some(b"run".as_slice()) {
            return None;
        }
        let argv = fields
            .map(|field| OsString::from_vec(field.to_vec()))
            .collect::<Vec<_>>();
        (!argv.is_empty()).then_some(Self::Run(argv))
    }
}

/// One window of a `list` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The ID the server gave this window, which is what `attach` takes.
    pub id: u64,
    /// What the client calls itself: its app ID, or its title without one.
    pub label: String,
    /// What the client calls the window, when it says.
    pub title: String,
    /// Whether this is the window with the keyboard.
    pub active: bool,
}

impl Window {
    /// One line of a `list`.
    ///
    /// Tabs and newlines in a name become spaces: the line break is what
    /// separates windows, and only the client decides what a name says.
    fn line(&self) -> String {
        let name = |name: &str| name.replace(['\t', '\n'], " ");
        format!(
            "{}\t{}\t{}\t{}\n",
            self.id,
            name(&self.label),
            name(&self.title),
            if self.active { "active" } else { "idle" }
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        Some(Self {
            id: fields.next()?.parse().ok()?,
            label: fields.next()?.to_owned(),
            title: fields.next()?.to_owned(),
            active: fields.next()? == "active",
        })
    }
}

/// What a server answers a command with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A `list`: every window, in the order they were mapped.
    Windows(Vec<Window>),
    /// A command that was carried out.
    Ok,
    /// A command that was refused, and why.
    Failed(String),
}

impl Reply {
    /// The reply to send.
    ///
    /// A window list says how long it is: a list of nothing and a server that
    /// answered nothing are otherwise the same bytes, and they are not the same
    /// answer.
    pub fn encode(&self) -> String {
        match self {
            Self::Windows(windows) => {
                let mut reply = format!("windows {}\n", windows.len());
                for window in windows {
                    reply.push_str(&window.line());
                }
                reply
            }
            Self::Ok => "ok\n".to_owned(),
            Self::Failed(reason) => format!("error: {reason}\n"),
        }
    }

    /// Read a reply.
    pub fn decode(response: &str) -> Self {
        if response == "ok\n" {
            return Self::Ok;
        }
        if let Some(reason) = response.strip_prefix("error: ") {
            return Self::Failed(reason.trim().to_owned());
        }
        let Some(header) = response.lines().next() else {
            // The server was there to accept the command and gone before it
            // answered, which is a refusal from where the CLI stands.
            return Self::Failed("the server stopped before it answered".to_owned());
        };
        let Some(count) = header
            .strip_prefix("windows ")
            .and_then(|count| count.parse::<usize>().ok())
        else {
            return Self::Failed("the server sent something unrecognised".to_owned());
        };
        let windows = response
            .lines()
            .skip(1)
            .filter_map(Window::parse)
            .collect::<Vec<_>>();
        if windows.len() == count {
            Self::Windows(windows)
        } else {
            Self::Failed("the window list was cut short".to_owned())
        }
    }
}

/// Ask a running server to carry out a command.
pub fn request(command: &Command) -> Result<Reply, Error> {
    let mut stream = connect(CONTROL_SOCKET)?;
    stream.write_all(&command.encode())?;
    // The server reads the request to the end, so closing this side of the
    // socket is what says the request is complete.
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(Reply::decode(&response))
}

/// Connect to one of the server's sockets, or say there is no server.
pub fn connect(name: &str) -> Result<UnixStream, Error> {
    let path = path(name)?;
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(stream),
        // A socket file that refuses the connection, or was taken out from
        // under it, is a server that is gone: what is on disk is a name left
        // behind, not a server.
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

fn path(name: &str) -> Result<PathBuf, Error> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDirectory)?;
    Ok(PathBuf::from(runtime).join(name))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    use super::{Command, Reply, Window};

    #[test]
    fn a_run_request_carries_an_argv_unchanged() {
        let argv = vec![
            OsString::from("foot"),
            OsString::from("-T"),
            OsString::from("two words"),
            OsString::from("line\nbreak\ttab"),
            OsString::from_vec(vec![0xff, 0xfe]),
        ];
        let encoded = Command::Run(argv.clone()).encode();
        assert_eq!(Command::decode(&encoded), Some(Command::Run(argv)));
    }

    #[test]
    fn a_run_request_needs_a_command() {
        assert_eq!(Command::decode(b"run"), None);
    }

    #[test]
    fn the_other_requests_survive_a_round_trip() {
        for command in [Command::List, Command::Quit] {
            assert_eq!(Command::decode(&command.encode()), Some(command));
        }
        assert_eq!(Command::decode(b""), None);
        assert_eq!(Command::decode(b"lately"), None);
    }

    #[test]
    fn a_window_list_survives_a_round_trip() {
        let windows = vec![
            Window {
                id: 1,
                label: "foot".to_owned(),
                title: "~ /code".to_owned(),
                active: true,
            },
            Window {
                id: 2,
                label: "two\tlines\nhere".to_owned(),
                title: "and\ttabs\nhere".to_owned(),
                active: false,
            },
            Window {
                id: 3,
                label: String::new(),
                title: String::new(),
                active: false,
            },
        ];
        let reply = Reply::Windows(windows.clone());
        let decoded = Reply::decode(&reply.encode());
        let mut expected = windows;
        expected[1].label = "two lines here".to_owned();
        expected[1].title = "and tabs here".to_owned();
        assert_eq!(decoded, Reply::Windows(expected));
    }

    #[test]
    fn no_windows_is_an_answer_and_no_bytes_is_not() {
        assert_eq!(
            Reply::decode(&Reply::Windows(Vec::new()).encode()),
            Reply::Windows(Vec::new())
        );
        assert_eq!(
            Reply::decode(""),
            Reply::Failed("the server stopped before it answered".to_owned())
        );
        assert_eq!(
            Reply::decode("2\n1\tfoot\t~ /code\tactive\n"),
            Reply::Failed("the server sent something unrecognised".to_owned())
        );
        assert_eq!(
            Reply::decode("windows 2\n1\tfoot\tactive\n"),
            Reply::Failed("the window list was cut short".to_owned())
        );
    }

    #[test]
    fn a_refusal_and_an_acceptance_survive_a_round_trip() {
        assert_eq!(Reply::decode(&Reply::Ok.encode()), Reply::Ok);
        assert_eq!(
            Reply::decode(&Reply::Failed("no window has that ID".to_owned()).encode()),
            Reply::Failed("no window has that ID".to_owned())
        );
    }
}
