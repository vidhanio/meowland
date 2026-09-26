use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, Read, Write},
};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 4;
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The length prefix every message carries, in bytes.
const HEADER: usize = 4;

thread_local! {
    /// Storage for one message, kept for the next one this thread sends or
    /// receives.  A frame is megabytes: allocating (and so faulting in) a
    /// buffer of that size per message is a per-frame cost on both sides.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// The modifier bits a pane packs into [`Input::Key`], one per modifier.
///
/// Both sides of the pane socket read these, so the layout lives here rather
/// than in either of them.
pub mod modifiers {
    pub const SHIFT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;

    /// Whether a key carrying these modifiers is a compositor binding.
    ///
    /// Alt alone is one, so a client can still receive the same key from any
    /// terminal that reports Ctrl or Super with it.
    #[must_use]
    pub const fn alt_only(modifiers: u8) -> bool {
        modifiers & (CONTROL | SUPER) == 0 && modifiers & ALT != 0
    }
}

/// The keys a pane treats as compositor bindings, in Linux input codes.
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Show {
    Id(u64),
    Newest,
    Focused,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Hello {
    pub version: u32,
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
        scroll: i16,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum PaneToServer {
    Hello(Hello),
    Input(Input),
    Resize {
        width: u32,
        height: u32,
    },
    /// The pane has taken the frame it was sent.  `drawn` says whether its
    /// terminal now shows it: a frame the pane dropped (because the terminal
    /// was still reading the last one) leaves the compositor's idea of the
    /// pane's screen stale, and it has to keep sending until one lands.
    Ack {
        drawn: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ServerToPane {
    HelloOk,
    Reject(String),
    /// The rows of a `width` by `height` frame from `y` down, `rgb` in
    /// `r, g, b` order: the pane composes bands into the frame it keeps, so a
    /// change that moves a cursor costs a row and not a frame.
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
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlRequest {
    Ping,
    Run(Vec<OsString>),
    List,
    Stop,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlResponse {
    Ok,
    Windows(Vec<WindowInfo>),
    Error(String),
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
                // The message outgrew the room; the next round has twice as much.
                Err(bincode::error::EncodeError::UnexpectedEnd) => {
                    let grown = (buffer.len() * 2).max(1024);
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
    let size = u32::from_le_bytes(header) as usize;
    if size > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    SCRATCH.with_borrow_mut(|buffer| {
        if buffer.len() < size {
            buffer.resize(size, 0);
        }
        reader.read_exact(&mut buffer[..size])?;
        let (value, used) = bincode::serde::decode_from_slice(
            &buffer[..size],
            bincode::config::standard().with_limit::<MAX_MESSAGE>(),
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if used != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing message bytes",
            ));
        }
        Ok(value)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated_oversized_and_trailing_frames() {
        let value = PaneToServer::Ack { drawn: true };
        let mut data = Vec::new();
        send(&mut data, &value).unwrap();
        assert!(recv::<PaneToServer>(&mut data[..data.len() - 1].as_ref()).is_err());

        let oversized = ((MAX_MESSAGE + 1) as u32).to_le_bytes().to_vec();
        assert!(recv::<PaneToServer>(&mut oversized.as_slice()).is_err());

        let mut trailing = data;
        let body_len = u32::from_le_bytes(trailing[..HEADER].try_into().unwrap());
        trailing[..HEADER].copy_from_slice(&(body_len + 1).to_le_bytes());
        trailing.push(0);
        assert!(recv::<PaneToServer>(&mut trailing.as_slice()).is_err());
    }

    #[test]
    fn sanitizes_controls_and_limits_characters() {
        assert_eq!(sanitize_with_limit("a\x1b猫b\n", 3), "a猫b");
    }

    /// The framing is a length prefix over a plain bincode encoding: both
    /// sides of the pane socket have to keep agreeing on it byte for byte, and
    /// the buffer `send` reuses must not leak one message into the next.
    #[test]
    fn framing_is_a_length_prefix_over_bincode() {
        let value = ServerToPane::Frame {
            width: 3,
            height: 2,
            y: 0,
            rgb: vec![1, 2, 3, 4, 5, 6],
        };
        let mut framed = Vec::new();
        send(&mut framed, &value).unwrap();
        let body = bincode::serde::encode_to_vec(
            &value,
            bincode::config::standard().with_limit::<MAX_MESSAGE>(),
        )
        .unwrap();
        let mut expected = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        expected.extend_from_slice(&body);
        assert_eq!(framed, expected);

        let mut small = Vec::new();
        send(&mut small, &PaneToServer::Ack { drawn: false }).unwrap();
        assert!(matches!(
            recv::<PaneToServer>(&mut small.as_slice()).unwrap(),
            PaneToServer::Ack { drawn: false }
        ));
    }
}
