use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

use rustix::{io::Errno, net::RecvFlags};
use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

const HEADER: usize = 4;

thread_local! {
    /// Reuse a message buffer across frames to avoid megabyte-scale allocations.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Shared modifier bits for [`Input::Key`].
pub mod modifiers {
    pub const SHIFT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;

    /// Whether a key carrying these modifiers is a compositor binding.
    ///
    /// Alt alone is one, so a client can still receive the same key from any
    /// terminal that reports `Ctrl` or `Super` with it.
    #[must_use]
    pub const fn alt_only(modifiers: u8) -> bool {
        modifiers & (CONTROL | SUPER) == 0 && modifiers & ALT != 0
    }
}

/// The keys a pane treats as compositor bindings, in Linux input codes.
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Show {
    Id(u64),
    Newest,
    Focused,
    /// Select the newest mapped window with this app ID, waiting if necessary.
    AppId(String),
    /// Select the window that redeems this launch's activation token.
    Activation(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub show: Show,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum Input {
    Key {
        /// Linux input code (`KEY_*`), which is 8 below what XKB uses for an
        /// `evdev` keymap; the conversion happens in the compositor alone.
        code: u16,
        pressed: bool,
        /// [`modifiers`] bits.
        modifiers: u8,
    },
    Pointer {
        x: f64,
        y: f64,
        button: Option<u8>,
        pressed: bool,
        /// Vertical wheel distance: 120 units is one detent; positive is up.
        scroll: i16,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PaneToServer {
    Hello(Hello),
    Input(Input),
    Resize {
        width: u32,
        height: u32,
    },
    /// `drawn` is false if the pane dropped the frame; the compositor must
    /// resend.
    Ack {
        drawn: bool,
    },
    Paste(String),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ServerToPane {
    HelloOk,
    Reject(String),
    /// RGB rows of a frame, starting at `y`.
    Frame {
        width: u32,
        height: u32,
        y: u32,
        #[serde(with = "serde_bytes")]
        rgb: Vec<u8>,
    },
    Release(String),
    Title(String),
    /// The pointer shape the terminal should show, or `None` for its own.
    Cursor(Option<String>),
    Clipboard(String),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlRequest {
    Ping,
    Run(Vec<OsString>),
    List,
    Kill(Show),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlResponse {
    Ok,
    Windows(Vec<WindowInfo>),
    Error(String),
    /// The client was launched with this `XDG_ACTIVATION_TOKEN`.
    Started(String),
}

/// Write one length-prefixed message.
///
/// # Errors
/// Returns an error when the message does not encode, when it is larger than
/// [`MAX_MESSAGE`], or when the writer fails.
pub fn send<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    SCRATCH.with_borrow_mut(|buffer| {
        loop {
            // The buffer's length is the room this thread remembers for a
            // message; it is never shortened, so a steady stream of
            // frames encodes into the same allocation.
            if buffer.len() < HEADER {
                buffer.resize(HEADER, 0);
            }
            let config = bincode::config::standard().with_limit::<MAX_MESSAGE>();
            match bincode::serde::encode_into_slice(value, &mut buffer[HEADER..], config) {
                Ok(size) => {
                    if size > MAX_MESSAGE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "message too large",
                        ));
                    }
                    buffer[..HEADER].copy_from_slice(&(size as u32).to_le_bytes());
                    return writer.write_all(&buffer[..HEADER + size]);
                }
                Err(bincode::error::EncodeError::UnexpectedEnd) => {
                    let maximum = HEADER + MAX_MESSAGE;
                    if buffer.len() == maximum {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "message too large",
                        ));
                    }
                    let grown = (buffer.len() * 2).clamp(1024, maximum);
                    buffer.reserve_exact(grown - buffer.len());
                    buffer.resize(grown, 0);
                }
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
    })
}

/// Read one length-prefixed message.
///
/// # Errors
/// Returns an error when the length prefix is missing, when it claims more
/// than [`MAX_MESSAGE`] bytes, when the body is truncated, or when it does not
/// decode as exactly one value of the requested type.
pub fn recv<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut header = [0; HEADER];
    reader.read_exact(&mut header)?;
    let size = message_size(header)?;
    SCRATCH.with_borrow_mut(|buffer| {
        if buffer.len() < size {
            buffer.resize(size, 0);
        }
        reader.read_exact(&mut buffer[..size])?;
        decode(&buffer[..size])
    })
}

fn message_size(header: [u8; HEADER]) -> io::Result<usize> {
    let size = u32::from_le_bytes(header) as usize;
    if size > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    Ok(size)
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    let (value, used) = bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<MAX_MESSAGE>(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if used != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing message bytes",
        ));
    }
    Ok(value)
}

/// One bounded, incremental message buffer for a readiness-driven socket.
///
/// The socket remains blocking for writes, including through cloned handles.
#[derive(Debug, Default)]
pub(crate) struct MessageReader {
    header: [u8; HEADER],
    header_read: usize,
    body: Vec<u8>,
    body_size: usize,
    body_read: usize,
}

impl MessageReader {
    /// Receive at most one message without waiting for missing bytes.
    ///
    /// `None` retains a partial packet for the next readable wakeup. Each call
    /// reads at most a header and 64 KiB of body, so larger packets yield to
    /// the event loop. EOF, oversized packets and decode errors match
    /// [`recv`].
    pub(crate) fn try_recv<T: for<'de> Deserialize<'de>>(
        &mut self,
        socket: &UnixStream,
    ) -> io::Result<Option<T>> {
        if self.header_read < HEADER {
            self.header_read += receive_available(socket, &mut self.header[self.header_read..])?;
            if self.header_read < HEADER {
                return Ok(None);
            }
            self.body_size = message_size(self.header)?;
            // Remember initialized room just as the blocking receiver does.
            if self.body.len() < self.body_size {
                self.body.resize(self.body_size, 0);
            }
        }
        if self.body_read < self.body_size {
            let end = (self.body_read + 64 * 1024).min(self.body_size);
            self.body_read += receive_available(socket, &mut self.body[self.body_read..end])?;
            if self.body_read < self.body_size {
                return Ok(None);
            }
        }
        self.header_read = 0;
        self.body_read = 0;
        decode(&self.body[..self.body_size]).map(Some)
    }
}

/// Return currently available bytes, or zero on would-block/interruption.
fn receive_available(socket: &UnixStream, bytes: &mut [u8]) -> io::Result<usize> {
    // O_NONBLOCK would also change cloned writers. DONTWAIT affects only this
    // receive call, leaving Hello, input and acknowledgements blocking.
    match rustix::net::recv(socket, bytes, RecvFlags::DONTWAIT) {
        Ok((0, _)) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "failed to fill whole buffer",
        )),
        Ok((read, _)) => Ok(read),
        Err(Errno::AGAIN | Errno::INTR) => Ok(0),
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Strip the control characters that would let a window title drive the
/// terminal, keeping at most [`SANITIZE`] characters.
#[must_use]
pub fn sanitize(input: &str) -> String {
    sanitize_with_limit(input, SANITIZE)
}

/// The length [`sanitize`] keeps.
const SANITIZE: usize = 256;

/// [`sanitize`] with the caller's own length limit.
#[must_use]
pub fn sanitize_with_limit(input: &str, limit: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
