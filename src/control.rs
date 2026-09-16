//! Control socket and command protocol.

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

use crate::Error;

pub const CONTROL_SOCKET: &str = "meowland-control";
pub const DISPLAY_SOCKET: &str = "meowland-display";

const ARGUMENT_SEPARATOR: u8 = 0;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    Run(Vec<OsString>),
    Stop,
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::List => b"list".to_vec(),
            Self::Stop => b"stop".to_vec(),
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

    pub fn decode(request: &[u8]) -> Option<Self> {
        if request == b"list" {
            return Some(Self::List);
        }
        if request == b"stop" {
            return Some(Self::Stop);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u64,
    pub label: String,
    pub title: String,
    pub active: bool,
}

impl Window {
    /// Remove controls before sending names to a terminal.
    fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\n",
            self.id,
            Self::clean_name(&self.label),
            Self::clean_name(&self.title),
            if self.active { "active" } else { "idle" }
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        Some(Self {
            id: fields.next()?.parse().ok()?,
            label: Self::clean_name(fields.next()?),
            title: Self::clean_name(fields.next()?),
            active: fields.next()? == "active",
        })
    }

    fn clean_name(name: &str) -> String {
        name.chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Windows(Vec<Window>),
    Ok,
    Failed(String),
}

impl Reply {
    /// Prefix the window list with its length to distinguish it from EOF.
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

    pub fn decode(response: &str) -> Self {
        if response == "ok\n" {
            return Self::Ok;
        }
        if let Some(reason) = response.strip_prefix("error: ") {
            return Self::Failed(reason.trim().to_owned());
        }
        let Some(header) = response.lines().next() else {
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

pub fn request(command: &Command) -> Result<Reply, Error> {
    let mut stream = connect(CONTROL_SOCKET)?;
    stream.write_all(&command.encode())?;
    // EOF marks the end of the request.
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
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
        for command in [Command::List, Command::Stop] {
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
            Window {
                id: 4,
                label: "bad\x1b[2J\rname".to_owned(),
                title: "control\u{7f}character".to_owned(),
                active: false,
            },
        ];
        let reply = Reply::Windows(windows.clone());
        let decoded = Reply::decode(&reply.encode());
        let mut expected = windows;
        expected[1].label = "two lines here".to_owned();
        expected[1].title = "and tabs here".to_owned();
        expected[3].label = "bad [2J name".to_owned();
        expected[3].title = "control character".to_owned();
        assert_eq!(decoded, Reply::Windows(expected));
    }

    #[test]
    fn a_window_list_from_an_older_server_cannot_print_terminal_controls() {
        let reply = Reply::decode("windows 1\n1\tapp\x1b[2J\ttitle\u{7f}\tidle\n");
        assert_eq!(
            reply,
            Reply::Windows(vec![Window {
                id: 1,
                label: "app [2J".to_owned(),
                title: "title ".to_owned(),
                active: false,
            }])
        );
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
