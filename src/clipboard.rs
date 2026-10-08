use std::{
    io::{self, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    string::FromUtf8Error,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use thiserror::Error;
use wl_clipboard_rs::{copy, paste};

pub const MAX_TEXT: usize = 1024 * 1024;
pub const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Utf8(#[from] FromUtf8Error),
    #[error(transparent)]
    Paste(#[from] paste::Error),
    #[error("clipboard text exceeds {MAX_TEXT} bytes")]
    TooLarge,
    #[error("clipboard transfer timed out")]
    Timeout,
}

#[derive(Debug)]
pub struct Transfer {
    fd: OwnedFd,
    bytes: Vec<u8>,
    deadline: Instant,
}

impl Transfer {
    pub(crate) fn new(fd: OwnedFd) -> Result<Self, ClipboardError> {
        fcntl_setfl(
            &fd,
            fcntl_getfl(&fd).map_err(io::Error::from)? | OFlags::NONBLOCK,
        )
        .map_err(io::Error::from)?;
        Ok(Self {
            fd,
            bytes: Vec::new(),
            deadline: Instant::now() + TRANSFER_TIMEOUT,
        })
    }

    pub(crate) fn receive(&mut self) -> Result<Option<String>, ClipboardError> {
        if Instant::now() >= self.deadline {
            return Err(ClipboardError::Timeout);
        }
        let mut bytes = [0; 16 * 1024];
        for _ in 0..4 {
            match rustix::io::read(&self.fd, &mut bytes) {
                Ok(0) => return Ok(Some(String::from_utf8(std::mem::take(&mut self.bytes))?)),
                Ok(read) => {
                    if self.bytes.len() + read > MAX_TEXT {
                        return Err(ClipboardError::TooLarge);
                    }
                    self.bytes.extend_from_slice(&bytes[..read]);
                }
                Err(Errno::AGAIN | Errno::INTR) => return Ok(None),
                Err(error) => return Err(io::Error::from(error).into()),
            }
        }
        Ok(None)
    }
}

#[derive(Debug)]
enum Request {
    Copy(String),
    Paste { code: u16, modifiers: u8 },
}

#[derive(Debug)]
pub enum Response {
    CopyFallback(String),
    Paste {
        code: u16,
        modifiers: u8,
        text: Option<String>,
    },
}

#[derive(Debug)]
pub struct HostClipboard {
    requests: SyncSender<Request>,
    responses: Receiver<Response>,
    pub(crate) wake: UnixStream,
    native: bool,
}

impl HostClipboard {
    pub(crate) fn new() -> io::Result<Self> {
        let (wake, mut notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        notify.set_nonblocking(true)?;
        let (requests, receiver) = mpsc::sync_channel(8);
        let (sender, responses) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("meowland-clipboard".into())
            .spawn(move || {
                for request in receiver {
                    let response = match request {
                        Request::Copy(text) => {
                            if let Err(error) = copy::Options::new().copy(
                                copy::Source::Bytes(text.as_bytes().into()),
                                copy::MimeType::Text,
                            ) {
                                tracing::debug!(%error, "Host clipboard copy failed; using OSC 52");
                                Some(Response::CopyFallback(text))
                            } else {
                                None
                            }
                        }
                        Request::Paste { code, modifiers } => {
                            let text = match read_host() {
                                Ok(text) => Some(text),
                                Err(error) => {
                                    tracing::debug!(%error, "Host clipboard paste unavailable");
                                    None
                                }
                            };
                            Some(Response::Paste {
                                code,
                                modifiers,
                                text,
                            })
                        }
                    };
                    if let Some(response) = response {
                        if sender.send(response).is_err() {
                            break;
                        }
                        let _ = notify.write_all(&[1]);
                    }
                }
            })?;
        let native = std::env::var_os("MEOWLAND_CLIPBOARD").is_none_or(|value| value != "terminal")
            && std::env::var_os("SSH_CONNECTION").is_none()
            && std::env::var_os("SSH_TTY").is_none();
        Ok(Self {
            requests,
            responses,
            wake,
            native,
        })
    }

    pub(crate) fn copy(&self, text: String) -> Option<String> {
        if !self.native {
            return Some(text);
        }
        match self.requests.try_send(Request::Copy(text)) {
            Ok(()) => None,
            Err(mpsc::TrySendError::Full(request) | mpsc::TrySendError::Disconnected(request)) => {
                match request {
                    Request::Copy(text) => Some(text),
                    Request::Paste { .. } => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn paste(&self, code: u16, modifiers: u8) -> bool {
        self.native
            && self
                .requests
                .try_send(Request::Paste { code, modifiers })
                .is_ok()
    }

    pub(crate) fn drain(&self) -> impl Iterator<Item = Response> + '_ {
        let mut bytes = [0; 64];
        while rustix::io::read(&self.wake, &mut bytes).is_ok_and(|read| read > 0) {}
        self.responses.try_iter()
    }
}

fn read_host() -> Result<String, ClipboardError> {
    let (pipe, _) = paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        paste::MimeType::Text,
    )?;
    let mut transfer = Transfer::new(pipe.into())?;
    loop {
        if let Some(text) = transfer.receive()? {
            return Ok(text);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn write_osc52(writer: &mut impl Write, text: &str) -> io::Result<()> {
    if text.len() > MAX_TEXT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard text too large",
        ));
    }
    write!(writer, "\x1b]52;c;{}\x1b\\", STANDARD.encode(text))?;
    writer.flush()
}
