//! Length-prefixed Postcard messages for control and pane sockets.

use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    time::Duration,
};

use meowland_core::pixels::FrameSize;
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
pub const RETAINED_BUFFER: usize = 1024 * 1024;

thread_local! {
    /// Reuse a message buffer across frames to avoid megabyte-scale allocations.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

bitflags::bitflags! {
    /// Shared modifier bits for [`Input::Key`].
    ///
    /// Values follow the Linux input convention, which differs from Kitty's wire
    /// order; [`Modifiers::from_kitty`] performs that translation.
    #[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
    #[serde(transparent)]
    pub struct Modifiers: u8 {
        /// Either Shift key is held.
        const SHIFT = 1;
        /// Either Control key is held.
        const CONTROL = 2;
        /// Either Alt key is held.
        const ALT = 4;
        /// The Super modifier is active.
        const SUPER = 8;
        /// The Hyper modifier is active.
        const HYPER = 16;
        /// The Meta modifier is active.
        const META = 32;
        /// Caps Lock is enabled.
        const CAPS_LOCK = 64;
        /// Num Lock is enabled.
        const NUM_LOCK = 128;
    }
}

/// Kitty's Alt bit, which this type numbers as [`Modifiers::ALT`].
const KITTY_ALT: u8 = 0b010;
/// Kitty's Control bit, which this type numbers as [`Modifiers::CONTROL`].
const KITTY_CONTROL: u8 = 0b100;

impl Modifiers {
    /// Modifiers that turn a key press into a shortcut rather than text.
    pub const SHORTCUT: Self = Self::CONTROL
        .union(Self::ALT)
        .union(Self::SUPER)
        .union(Self::HYPER)
        .union(Self::META);

    /// Translate Kitty's modifier bit layout into this type.
    ///
    /// Kitty reports Alt as bit 1 (value 2) and Control as bit 2 (value 4),
    /// while this type numbers Control as 2 and Alt as 4. Every other bit
    /// already matches, so only those two swap.
    #[must_use]
    pub const fn from_kitty(bits: u8) -> Self {
        let mut modifiers = Self::from_bits_retain(bits & !(KITTY_ALT | KITTY_CONTROL));
        if bits & KITTY_ALT != 0 {
            modifiers = modifiers.union(Self::ALT);
        }
        if bits & KITTY_CONTROL != 0 {
            modifiers = modifiers.union(Self::CONTROL);
        }
        modifiers
    }
}

/// One compositor key binding: a Kitty key identity plus required modifiers.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    /// Kitty key identity: an unshifted Unicode code point or a functional key
    /// number.
    pub key: u32,
    /// Modifiers that must be held, excluding Caps Lock and Num Lock.
    pub modifiers: Modifiers,
}

impl KeyBinding {
    /// Whether a key transition matches this binding.
    ///
    /// Required modifiers must be held. Extra Shift and lock state do not
    /// matter, but any other shortcut modifier prevents a match.
    #[must_use]
    pub const fn matches(self, key: u32, modifiers: Modifiers) -> bool {
        let lock = Modifiers::CAPS_LOCK.union(Modifiers::NUM_LOCK);
        let held = modifiers.difference(lock);
        let required = self.modifiers.difference(lock);
        self.key == key
            && held.contains(required)
            && !held.intersects(Modifiers::SHORTCUT.difference(required))
    }
}

/// Compositor bindings every pane intercepts before forwarding input.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Shortcuts {
    /// Ask the displayed Wayland window to close; `None` disables the binding.
    pub close_window: Option<KeyBinding>,
    /// Release the terminal pane without closing its window; `None` disables
    /// the binding.
    pub detach_pane: Option<KeyBinding>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            close_window: Some(KeyBinding {
                key: u32::from(b'q'),
                modifiers: Modifiers::ALT,
            }),
            detach_pane: Some(KeyBinding {
                key: u32::from(b'w'),
                modifiers: Modifiers::ALT,
            }),
        }
    }
}

impl Shortcuts {
    /// Resolve a key transition to the configured shortcut action.
    #[must_use]
    pub fn action(self, key: u32, modifiers: Modifiers) -> Option<Shortcut> {
        if self
            .close_window
            .is_some_and(|binding| binding.matches(key, modifiers))
        {
            Some(Shortcut::CloseWindow)
        } else if self
            .detach_pane
            .is_some_and(|binding| binding.matches(key, modifiers))
        {
            Some(Shortcut::DetachPane)
        } else {
            None
        }
    }
}

/// Kitty key identity for V, used by the clipboard paste binding.
pub const KEY_V: u32 = 0x76;
/// Kitty functional-key number for Insert, used by the alternate paste
/// binding.
pub const KEY_INSERT: u32 = 57348;

/// Compositor shortcut actions shared by terminal release interception and
/// dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Ask the displayed Wayland window to close.
    CloseWindow,
    /// Release the terminal pane without closing its window.
    DetachPane,
}

/// Whether retained pixels remain a valid basis for subsequent frame bands.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum FrameAck {
    /// The pane retained the update, including updates not yet displayed.
    Retained,
    /// The pane requires a complete frame before accepting further bands.
    Resync,
}

/// Server-assigned identity of a window, stable for its lifetime.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct WindowId(u64);

impl WindowId {
    /// Numeric identity as assigned by the compositor.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for WindowId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Server-assigned identity of one terminal clipboard read.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct ClipboardRequestId(u64);

impl ClipboardRequestId {
    /// Numeric identity as assigned by the compositor.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for ClipboardRequestId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Terminal-assigned identity of one clipboard selection offer.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct ClipboardOfferId(u64);

impl ClipboardOfferId {
    /// Numeric identity as assigned by the terminal.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for ClipboardOfferId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Window selector used by attachment, launch, and control requests.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Show {
    /// Select a particular server-assigned window ID.
    Id(WindowId),
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
    pub id: WindowId,
    /// Application identity advertised by the Wayland client.
    pub app_id: String,
    /// Client-provided title; sanitize before emitting terminal control
    /// sequences.
    pub title: String,
    /// Whether this window currently has compositor focus.
    pub state: WindowState,
}

/// Compositor focus state of one window.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum WindowState {
    /// The window currently has compositor focus.
    Active,
    /// Another window or no window has compositor focus.
    Inactive,
}

/// Initial pane request, sent before any [`PaneAction`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hello {
    /// Pane bounds in physical pixels, validated during deserialization.
    pub size: FrameSize,
    /// Window selection to bind to this pane.
    pub show: Show,
}

/// Pointer button identity, using X11 button numbers for extra buttons.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Button {
    /// Primary (left) button.
    Left,
    /// Middle button.
    Middle,
    /// Secondary (right) button.
    Right,
    /// Backward side button, X11 button 8.
    Back,
    /// Forward side button, X11 button 9.
    Forward,
    /// Any other button, identified by its X11 button number.
    Other(u8),
}

/// Whether a key or pointer button is going down or coming up.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Transition {
    /// The key or button is now held down.
    Pressed,
    /// A previously held key or button was let go.
    Released,
}

/// Wheel movement in signed 120-unit detents.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Scroll {
    /// Vertical distance; positive scrolls up and negative scrolls down.
    pub vertical: i16,
    /// Horizontal distance; positive scrolls right and negative scrolls left.
    pub horizontal: i16,
}

/// Terminal-originated input; key identities follow the Kitty keyboard
/// protocol rather than Linux evdev codes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Input {
    /// One key transition with the terminal's authoritative modifier snapshot.
    Key {
        /// Kitty key identity: an unshifted Unicode code point, or a functional
        /// key number from 57344 upward.
        key: u32,
        /// A real key transition. The key stays down until its release or a
        /// reset.
        state: Transition,
        /// [`Modifiers`] snapshot, including lock state.
        modifiers: Modifiers,
        /// Text the terminal associated with the transition, when it reported
        /// any; at most [`MAX_INPUT_TEXT`] bytes and without control
        /// characters.
        text: Option<String>,
        /// Client-known held keys immediately before this transition.
        held: Vec<u32>,
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
        /// Terminal button transition, or no transition for plain motion.
        button: Option<(Button, Transition)>,
        /// Wheel movement since the previous event; 120 units is one detent.
        scroll: Scroll,
    },
    /// Release the pane's keyboard state after terminal focus loss.
    ResetKeyboard,
    /// Client-known held keys and modifiers on focus entry.
    KeyboardEnter {
        /// Kitty key identities held when focus was gained.
        keys: Vec<u32>,
        /// [`Modifiers`] snapshot, including lock state.
        modifiers: Modifiers,
    },
    /// Committed Unicode text that arrived without a key identity.
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

/// Why a terminal published a clipboard offer.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum OfferIntent {
    /// Refresh MIME metadata without triggering paste.
    Refresh,
    /// Publish the offer and trigger the application's paste shortcut.
    Paste,
}

/// Actions accepted after the pane handshake, independent of socket ownership.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PaneAction {
    /// Forward a decoded terminal input event.
    Input(Input),
    /// Change the pane's physical-pixel bounds.
    Resize {
        /// New physical-pixel bounds, validated during deserialization.
        size: FrameSize,
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
        offer: ClipboardOfferId,
        /// Available MIME types, validated and bounded by the receiver.
        mimes: Vec<String>,
        /// What accepting the offer should do.
        intent: OfferIntent,
    },
    /// Complete a previously requested terminal clipboard read.
    ClipboardReply {
        /// Server-assigned request identity.
        request: ClipboardRequestId,
        /// Raw bytes on success, or `None` if unavailable or denied.
        #[serde(with = "serde_bytes")]
        data: Option<Vec<u8>>,
    },
    /// Report the clipboard transport the terminal accepted while probing.
    ///
    /// The server reads only representations the pane can apply, so a legacy
    /// OSC 52 pane never pays for image transfers.
    ClipboardTransport(crate::clipboard::Transport),
    /// Select pixel transport after terminal probing or shared-memory failure.
    FrameTransport(crate::FrameTransport),
}

/// Server packets share one schema for owned frames and split frame headers.
#[derive(Debug, Deserialize, Serialize)]
pub enum PanePacket<F> {
    /// The initial handshake succeeded with the compositor's active bindings.
    HelloOk {
        /// Shortcuts the pane must intercept before forwarding key events.
        shortcuts: Shortcuts,
    },
    /// The handshake was rejected with a human-readable reason.
    Reject(String),
    /// A validated RGB rectangle, or its explicit header during split
    /// transmission.
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
        request: ClipboardRequestId,
        /// Terminal-assigned identity of the selection offer.
        offer: ClipboardOfferId,
        /// Requested MIME representation.
        mime: String,
        /// Shared monotonic deadline, including time spent awaiting permission.
        deadline: crate::clipboard::Deadline,
    },
    /// Discard a pending read and suppress any eventual reply.
    ClipboardCancel {
        /// Server-assigned identity of the cancelled read.
        request: ClipboardRequestId,
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

/// Trim a handled buffer back to [`RETAINED_BUFFER`].
///
/// Buffers grow to the largest message they have carried, so this is called
/// once that message has been written or decoded, or after clearing a buffer
/// whose largest contents are gone. The retained window stays usable, so only
/// messages above the threshold pay for a fresh allocation on the way back up.
pub fn release_oversized(buffer: &mut Vec<u8>) {
    if buffer.len() > RETAINED_BUFFER {
        buffer.truncate(RETAINED_BUFFER);
    }
    buffer.shrink_to(RETAINED_BUFFER);
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
