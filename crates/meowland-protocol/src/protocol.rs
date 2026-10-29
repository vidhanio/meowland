//! Length-prefixed Postcard messages for control and pane sockets.

use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Maximum encoded packet body; bounds allocations made for untrusted length
/// prefixes.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// Time allowed for a local control request or initial pane handshake.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum UTF-8 bytes in a single committed keyboard-text event.
pub const MAX_INPUT_TEXT: usize = 4096;

pub(crate) const HEADER: usize = 4;

/// Largest framing buffer retained for reuse after a message is handled.
///
/// Buffers grow to the largest message seen. A single maximum-size frame would
/// otherwise pin `MAX_MESSAGE` bytes for the life of the process, so anything
/// above this is trimmed once its message has been written or decoded.
pub(crate) const RETAINED_BUFFER: usize = 1024 * 1024;

thread_local! {
    /// Reuse a message buffer across frames to avoid megabyte-scale allocations.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Shared modifier bits for [`Input::Key`].
pub mod modifiers {
    /// Either Shift key is held.
    pub const SHIFT: u8 = 1;
    /// Either Control key is held.
    pub const CONTROL: u8 = 2;
    /// Either Alt key is held.
    pub const ALT: u8 = 4;
    /// The Super modifier is active.
    pub const SUPER: u8 = 8;
    /// The Hyper modifier is active.
    pub const HYPER: u8 = 16;
    /// The Meta modifier is active.
    pub const META: u8 = 32;
    /// Caps Lock is enabled.
    pub const CAPS_LOCK: u8 = 64;
    /// Num Lock is enabled.
    pub const NUM_LOCK: u8 = 128;

    /// Whether a key carrying these modifiers is a compositor binding.
    ///
    /// Shift and lock state are allowed; other shortcut modifiers are not.
    #[must_use]
    pub const fn alt_only(modifiers: u8) -> bool {
        modifiers & (CONTROL | SUPER | HYPER | META) == 0 && modifiers & ALT != 0
    }
}

/// Linux input code for Q, used by the close-window binding.
pub const KEY_Q: u16 = 16;
/// Linux input code for W, used by the detach-pane binding.
pub const KEY_W: u16 = 17;
/// Linux input code for V, used by the clipboard paste binding.
pub const KEY_V: u16 = 47;
/// Linux input code for Insert, used by the alternate paste binding.
pub const KEY_INSERT: u16 = 110;

/// Compositor bindings shared by terminal release interception and dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Ask the displayed Wayland window to close.
    CloseWindow,
    /// Release the terminal pane without closing its window.
    DetachPane,
}

impl Shortcut {
    /// Recognize an Alt binding from a Linux key code and modifier snapshot.
    #[must_use]
    pub const fn from_key(code: u16, bits: u8) -> Option<Self> {
        if !modifiers::alt_only(bits) {
            return None;
        }
        match code {
            KEY_Q => Some(Self::CloseWindow),
            KEY_W => Some(Self::DetachPane),
            _ => None,
        }
    }
}

/// Whether retained pixels remain a valid basis for subsequent frame bands.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum FrameAck {
    /// The pane retained the update, including updates not yet displayed.
    Retained,
    /// The pane requires a complete frame before accepting further bands.
    Resync,
}

/// Window selector used by attachment, launch, and control requests.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Show {
    /// Select a particular server-assigned window ID.
    Id(u64),
    /// Follow the newest mapped window.
    Newest,
    /// Select the currently focused window.
    Focused,
    /// Select the newest mapped window with this app ID, waiting if necessary.
    AppId(String),
    /// Select the window that redeems this launch's activation token.
    Activation(String),
}

/// Window metadata returned by the server's list operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    /// Server-assigned window ID, stable for the window's lifetime.
    pub id: u64,
    /// Application identity advertised by the Wayland client.
    pub app_id: String,
    /// Client-provided title; sanitize before emitting terminal control
    /// sequences.
    pub title: String,
    /// Whether this window currently has compositor focus.
    pub active: bool,
}

/// Initial pane request, sent before any [`PaneAction`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hello {
    /// Pane width in physical pixels, validated by the server.
    pub width: u32,
    /// Pane height in physical pixels, validated by the server.
    pub height: u32,
    /// Window selection to bind to this pane.
    pub show: Show,
}

/// Terminal-originated input; key codes use Linux evdev identities, not XKB
/// offsets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Input {
    /// One key transition with the terminal's authoritative modifier snapshot.
    Key {
        /// Linux input code (`KEY_*`), eight below the XKB evdev key code.
        code: u16,
        /// A real key transition. It stays down until its release or a reset.
        pressed: bool,
        /// [`modifiers`] snapshot, including lock state.
        modifiers: u8,
        /// Client-known held keys immediately before this transition.
        held: Vec<u16>,
    },
    /// Pointer motion, an optional button transition, and vertical wheel
    /// movement.
    Pointer {
        /// Horizontal pane pixel coordinate, possibly outside the pane during a
        /// drag.
        x: f64,
        /// Vertical pane pixel coordinate, possibly outside the pane during a
        /// drag.
        y: f64,
        /// Terminal button identity, or no button transition for plain motion.
        button: Option<u8>,
        /// Whether the optional button was pressed rather than released.
        pressed: bool,
        /// Vertical wheel distance: 120 units is one detent; positive is up.
        scroll: i16,
    },
    /// Release the pane's keyboard state after terminal focus loss.
    ResetKeyboard,
    /// Client-known held keys and modifiers on focus entry.
    KeyboardEnter {
        /// Linux input codes held when focus was gained.
        keys: Vec<u16>,
        /// [`modifiers`] snapshot, including lock state.
        modifiers: u8,
    },
    /// Committed Unicode text that has no usable Linux key code.
    Text(String),
}

/// Messages sent over a pane connection, starting with exactly one handshake.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PaneToServer {
    /// Establish pane geometry and window selection.
    Hello(Hello),
    /// Perform an action after a successful handshake.
    Action(PaneAction),
}

/// Actions accepted after the pane handshake, independent of socket ownership.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PaneAction {
    /// Forward a decoded terminal input event.
    Input(Input),
    /// Change the pane's physical-pixel bounds.
    Resize {
        /// New physical-pixel width.
        width: u32,
        /// New physical-pixel height.
        height: u32,
    },
    /// Acknowledge retained pixels or request resynchronization.
    Ack(FrameAck),
    /// Publish UTF-8 text and trigger the application's paste shortcut.
    Paste(String),
    /// Publish MIME types available from a terminal selection.
    ClipboardOffer {
        /// Clipboard or primary selection being replaced.
        selection: crate::clipboard::Selection,
        /// Terminal-assigned offer identity for subsequent reads.
        offer: u64,
        /// Available MIME types, validated and bounded by the receiver.
        mimes: Vec<String>,
        /// Whether accepting the offer should also trigger paste.
        paste: bool,
    },
    /// Complete a previously requested terminal clipboard read.
    ClipboardReply {
        /// Server-assigned request identity.
        request: u64,
        /// Raw bytes on success, or `None` if unavailable or denied.
        #[serde(with = "serde_bytes")]
        data: Option<Vec<u8>>,
    },
    /// Select pixel transport after terminal probing or shared-memory failure.
    FrameTransport(crate::FrameTransport),
}

/// Server packets share one schema for owned frames and split frame headers.
#[derive(Debug, Deserialize, Serialize)]
pub enum PanePacket<F> {
    /// The initial handshake succeeded.
    HelloOk,
    /// The handshake was rejected with a human-readable reason.
    Reject(String),
    /// A validated RGB band, or its explicit header during split transmission.
    Frame(F),
    /// The pane is detached; restore terminal modes and report this reason.
    Release(String),
    /// Replace the terminal title after sanitizing control characters.
    Title(String),
    /// The pointer shape the terminal should show, or `None` for its own.
    Cursor(Option<String>),
    /// Copy application-provided data into the terminal selection.
    ClipboardWrite {
        /// Selection receiving the copied data.
        selection: crate::clipboard::Selection,
        /// Bounded representations of the selection.
        data: crate::clipboard::ClipboardData,
    },
    /// Request one MIME representation from a previously announced offer.
    ClipboardRead {
        /// Server-assigned identity used in the reply or cancellation.
        request: u64,
        /// Terminal-assigned identity of the selection offer.
        offer: u64,
        /// Requested MIME representation.
        mime: String,
        /// Shared monotonic deadline, including time spent awaiting permission.
        deadline: crate::clipboard::Deadline,
    },
    /// Discard a pending read and suppress any eventual reply.
    ClipboardCancel {
        /// Server-assigned identity of the cancelled read.
        request: u64,
    },
    /// Ring the terminal's audible or visual bell.
    Bell,
    /// A complete immutable RGB object, requiring shared-memory negotiation.
    SharedFrame(crate::SharedFrame),
}

/// Server packet carrying owned, validated RGB frame bands.
pub type ServerToPane = PanePacket<crate::FrameBand>;

/// One-shot request sent over the control socket.
#[derive(Debug, Deserialize, Serialize)]
pub enum ControlRequest {
    /// Check whether the server is responsive.
    Ping,
    /// Launch a process connected to the compositor.
    Run {
        /// Executable followed by its arguments, preserving non-UTF-8 OS
        /// strings.
        args: Vec<OsString>,
        /// Environment entries to apply to the launched process.
        env: Vec<(OsString, OsString)>,
        /// Working directory of the launched process.
        cwd: PathBuf,
    },
    /// List mapped windows and their metadata.
    List,
    /// Request closure of the selected window.
    Kill(Show),
}

/// Response to one control request; failures retain a user-facing explanation.
#[derive(Debug, Deserialize, Serialize)]
pub enum ControlResponse {
    /// The operation completed without a payload.
    Ok,
    /// Current mapped-window metadata.
    Windows(Vec<WindowInfo>),
    /// The operation failed with this explanation.
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
        let result = write_message(buffer, writer, value);
        release_oversized(buffer);
        result
    })
}

fn write_message<T: Serialize>(
    buffer: &mut Vec<u8>,
    writer: &mut impl Write,
    value: &T,
) -> io::Result<()> {
    loop {
        if buffer.len() < HEADER {
            buffer.resize(HEADER, 0);
        }
        match postcard::to_slice(value, &mut buffer[HEADER..]).map(|encoded| encoded.len()) {
            Ok(size) => {
                let length = u32::try_from(size)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                buffer[..HEADER].copy_from_slice(&length.to_le_bytes());
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
        let result = read_message(buffer, reader, size);
        release_oversized(buffer);
        result
    })
}

fn read_message<T: for<'de> Deserialize<'de>>(
    buffer: &mut Vec<u8>,
    reader: &mut impl Read,
    size: usize,
) -> io::Result<T> {
    if buffer.len() < size {
        buffer.resize(size, 0);
    }
    reader.read_exact(&mut buffer[..size])?;
    decode(&buffer[..size])
}

/// Trim a framing buffer back to [`RETAINED_BUFFER`] after a larger message.
///
/// The retained window stays usable, so only messages above the threshold pay
/// for a fresh allocation on the way back up.
pub(crate) fn release_oversized(buffer: &mut Vec<u8>) {
    if buffer.len() > RETAINED_BUFFER {
        buffer.truncate(RETAINED_BUFFER);
        buffer.shrink_to(RETAINED_BUFFER);
    }
}

pub(crate) fn message_size(header: [u8; HEADER]) -> io::Result<usize> {
    let size = usize::try_from(u32::from_le_bytes(header))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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

/// Strip terminal control characters, keeping at most 256 Unicode characters.
#[must_use]
pub fn sanitize(input: &str) -> String {
    sanitize_with_limit(input, SANITIZE)
}

/// The length [`sanitize`] keeps.
const SANITIZE: usize = 256;

/// Strip terminal control characters, retaining at most `limit` Unicode
/// characters.
#[must_use]
pub fn sanitize_with_limit(input: &str, limit: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
