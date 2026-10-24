//! Length-prefixed Postcard messages for control and pane sockets.

use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
};

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
    pub const HYPER: u8 = 16;
    pub const META: u8 = 32;
    pub const CAPS_LOCK: u8 = 64;
    pub const NUM_LOCK: u8 = 128;

    /// Whether a key carrying these modifiers is a compositor binding.
    ///
    /// Shift and lock state are allowed; other shortcut modifiers are not.
    #[must_use]
    pub const fn alt_only(modifiers: u8) -> bool {
        modifiers & (CONTROL | SUPER | HYPER | META) == 0 && modifiers & ALT != 0
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Input {
    Key {
        /// Linux input code (`KEY_*`), which is 8 below what XKB uses for an
        /// `evdev` keymap; the conversion happens in the compositor alone.
        code: u16,
        /// A real key transition. It stays down until its release or a reset.
        pressed: bool,
        /// [`modifiers`] snapshot, including lock state.
        modifiers: u8,
        /// Client-known held keys immediately before this transition.
        held: Vec<u16>,
    },
    Pointer {
        /// Zero-based pane pixel coordinates, including positions outside the
        /// pane during a drag.
        x: f64,
        y: f64,
        button: Option<u8>,
        pressed: bool,
        /// Vertical wheel distance: 120 units is one detent; positive is up.
        scroll: i16,
    },
    /// Release the pane's keyboard state after terminal focus loss.
    ResetKeyboard,
    /// Client-known held keys and modifiers on focus entry.
    KeyboardEnter { keys: Vec<u16>, modifiers: u8 },
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
    ClipboardOffer {
        selection: crate::clipboard::Selection,
        offer: u64,
        mimes: Vec<String>,
        paste: bool,
    },
    ClipboardReply {
        request: u64,
        #[serde(with = "serde_bytes")]
        data: Option<Vec<u8>>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ServerToPane {
    HelloOk,
    Reject(String),
    /// RGB rows of a frame, starting at `y`.
    /// The RGB byte string stays last so writers can transmit its storage
    /// directly.
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
    ClipboardWrite {
        selection: crate::clipboard::Selection,
        data: crate::clipboard::ClipboardData,
    },
    ClipboardRead {
        request: u64,
        offer: u64,
        mime: String,
        deadline: crate::clipboard::Deadline,
    },
    ClipboardCancel {
        request: u64,
    },
    Bell,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlRequest {
    Ping,
    Run {
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    },
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
            if buffer.len() < HEADER {
                buffer.resize(HEADER, 0);
            }
            match postcard::to_slice(value, &mut buffer[HEADER..]).map(|encoded| encoded.len()) {
                Ok(size) => {
                    buffer[..HEADER].copy_from_slice(&(size as u32).to_le_bytes());
                    return writer.write_all(&buffer[..HEADER + size]);
                }
                Err(postcard::Error::SerializeBufferFull) => {
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

/// Decode one packet body without its length prefix.
///
/// # Errors
/// Returns an error for oversized, malformed, or trailing message bytes.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let (value, remaining) = postcard::take_from_bytes(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !remaining.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing message bytes",
        ));
    }
    Ok(value)
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
