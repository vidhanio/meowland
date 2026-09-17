//! The pane protocol: what a terminal pane and the server say to each other.
//!
//! One terminal holds one end of a socket to the server, and this is what
//! crosses it. Every message is a kind, a length, and a body: the body of a
//! structured message is bincode, and the body of a frame or an escape is the
//! bytes themselves, which are already what the terminal or the compositor
//! wants.
//!
//! The kind is what lets a message this build does not know be skipped whole,
//! rather than ending the conversation: a build that adds one can still talk to
//! a peer that has not learned it.

use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
};

use evdev::KeyCode;
use serde::{Deserialize, Serialize};

use crate::protocol::{ProtocolVersion, WindowId, control, decode, encode};

pub fn listen() -> Result<(control::Socket, UnixListener), crate::Error> {
    control::Socket::bind(control::DISPLAY_SOCKET)
}

pub fn connect() -> Result<UnixStream, crate::Error> {
    control::connect(control::DISPLAY_SOCKET)
}

/// The version of this protocol.
///
/// Bump it whenever these messages change. A pane and a server that disagree
/// are then refused with a reason to show, rather than half-understood.
pub const VERSION: ProtocolVersion = ProtocolVersion::new(3);

/// What the terminal reported it can do. Each field is one independent answer.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each of these is an independent thing a terminal can do"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The size of one character cell, in pixels.
    pub cell: (u32, u32),
    /// The terminal's character grid: `(columns, rows)` in cells.
    pub cells: (u32, u32),
    /// `(width, height)` of the whole drawing area in pixels.
    pub pixels: (u32, u32),
    pub terminal: Option<String>,
    pub graphics: bool,
    pub keyboard: bool,
    /// Whether mouse reporting uses pixels (`SGR-Pixels`) instead of cells.
    pub pixel_mouse: bool,
    /// Whether the terminal reads tiles out of shared memory, which keeps their
    /// pixels off the pty.
    pub shared_memory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Show {
    /// The window the server has focused, resolved when the pane attaches.
    Focused,
    /// The newest window, and each new one after it.
    Newest,
    /// This window, by the ID the server gave it.
    Window(WindowId),
}

/// The first message a pane sends: who it is, and which window it wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: ProtocolVersion,
    pub show: Show,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToServer {
    Hello(Hello),
    Resized(Capabilities),
    Input(Input),
    /// The terminal has written the last frame, so the server may compose the
    /// next frame.
    Drawn,
    Bye,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToClient {
    Welcome,
    Bytes(Vec<u8>),
    /// Acknowledged with [`ToServer::Drawn`].
    Frame(Vec<u8>),
    Detached(String),
}

impl ToClient {
    /// Put escapes on the wire.
    ///
    /// They are the terminal's own bytes already, so they are written as they
    /// are: see [`ToClient::write_frame`].
    pub fn write_bytes<W: io::Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
        write_message(writer, Kind::Bytes, bytes)
    }

    /// Put a frame on the wire.
    ///
    /// The presenter built it as the bytes the terminal takes, and they go out
    /// of that buffer: nothing is copied, and nothing is encoded.
    pub fn write_frame<W: io::Write>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
        write_message(writer, Kind::Frame, frame)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    Key(Key),
    Pointer(Pointer),
    Paste(String),
    Focus(bool),
}

/// Key code and modifier state sent to Wayland clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// A Linux input event code (`KEY_*`).
    #[serde(with = "key_code")]
    pub code: KeyCode,
    /// Whether the symbol this key stands for needs shift. The terminal side
    /// has this, and the key code does not.
    pub shift: bool,
    /// The modifier state the terminal reported, in `KeyModifiers` bits.
    pub modifiers: u8,
    pub kind: KeyKind,
    /// Whether this key is a modifier. The press and release of a modifier are
    /// state for the client, not keystrokes.
    pub modifier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pointer {
    Motion {
        column: u16,
        row: u16,
    },
    Button {
        column: u16,
        row: u16,
        /// The evdev button code, which the compositor forwards.
        #[serde(with = "key_code")]
        button: KeyCode,
        pressed: bool,
    },
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// The kind of every message, in both directions.
///
/// The byte of each kind is written out rather than taken from the order, so
/// that adding a message cannot change what an older build hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Hello,
    Input,
    Resized,
    Drawn,
    Bye,
    Welcome,
    Bytes,
    Detached,
    Frame,
}

impl Kind {
    const fn byte(self) -> u8 {
        match self {
            Self::Hello => 1,
            Self::Input => 2,
            Self::Resized => 3,
            Self::Drawn => 4,
            Self::Bye => 5,
            Self::Welcome => 6,
            Self::Bytes => 7,
            Self::Detached => 8,
            Self::Frame => 9,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Hello,
            2 => Self::Input,
            3 => Self::Resized,
            4 => Self::Drawn,
            5 => Self::Bye,
            6 => Self::Welcome,
            7 => Self::Bytes,
            8 => Self::Detached,
            9 => Self::Frame,
            _ => return None,
        })
    }
}

impl From<&ToServer> for Kind {
    fn from(message: &ToServer) -> Self {
        match message {
            ToServer::Hello(_) => Self::Hello,
            ToServer::Resized(_) => Self::Resized,
            ToServer::Input(_) => Self::Input,
            ToServer::Drawn => Self::Drawn,
            ToServer::Bye => Self::Bye,
        }
    }
}

/// Put a message on the wire.
pub fn write_to<W: io::Write>(writer: &mut W, message: &ToServer) -> io::Result<()> {
    write_message(writer, Kind::from(message), &body(message)?)
}

/// The next message a pane sent, or `None` when it has nothing more to say.
pub fn read_from<R: io::Read>(reader: &mut R) -> io::Result<Option<ToServer>> {
    loop {
        let Some((kind, body)) = read_message(reader)? else {
            return Ok(None);
        };
        match kind {
            Kind::Hello => return Ok(Some(ToServer::Hello(decode(&body)?))),
            Kind::Resized => return Ok(Some(ToServer::Resized(decode(&body)?))),
            Kind::Input => return Ok(Some(ToServer::Input(decode(&body)?))),
            Kind::Drawn => return Ok(Some(ToServer::Drawn)),
            Kind::Bye => return Ok(Some(ToServer::Bye)),
            // A message from a newer build, or one meant for the other
            // direction: it is skipped whole, because the length in front of it
            // says where it ends.
            Kind::Welcome | Kind::Bytes | Kind::Detached | Kind::Frame => {}
        }
    }
}

/// Put a message to a pane on the wire.
pub fn write_for_pane<W: io::Write>(writer: &mut W, message: &ToClient) -> io::Result<()> {
    match message {
        ToClient::Welcome => write_message(writer, Kind::Welcome, &[]),
        ToClient::Bytes(bytes) => ToClient::write_bytes(writer, bytes),
        ToClient::Frame(frame) => ToClient::write_frame(writer, frame),
        ToClient::Detached(reason) => write_message(writer, Kind::Detached, &encode(reason)?),
    }
}

/// The next message the server sent a pane, or `None` when it has nothing more
/// to say.
pub fn read_for_pane<R: io::Read>(reader: &mut R) -> io::Result<Option<ToClient>> {
    loop {
        let Some((kind, body)) = read_message(reader)? else {
            return Ok(None);
        };
        match kind {
            Kind::Welcome => return Ok(Some(ToClient::Welcome)),
            Kind::Bytes => return Ok(Some(ToClient::Bytes(body))),
            Kind::Frame => return Ok(Some(ToClient::Frame(body))),
            Kind::Detached => return Ok(Some(ToClient::Detached(decode(&body)?))),
            Kind::Hello | Kind::Input | Kind::Resized | Kind::Drawn | Kind::Bye => {}
        }
    }
}

/// The body of a message, which is the message itself in bincode.
///
/// A message with nothing to say has no body: its kind has said it all.
fn body(message: &ToServer) -> io::Result<Vec<u8>> {
    match message {
        ToServer::Hello(hello) => encode(hello),
        ToServer::Resized(capabilities) => encode(capabilities),
        ToServer::Input(input) => encode(input),
        ToServer::Drawn | ToServer::Bye => Ok(Vec::new()),
    }
}

/// Write a kind, the length of its body, and the body.
fn write_message<W: io::Write>(writer: &mut W, kind: Kind, body: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind.byte();
    header[1..].copy_from_slice(&(body.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(body)?;
    writer.flush()
}

fn read_message<R: io::Read>(reader: &mut R) -> io::Result<Option<(Kind, Vec<u8>)>> {
    loop {
        let mut header = [0u8; 5];
        if !read_exactly(reader, &mut header)? {
            return Ok(None);
        }
        let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length > crate::protocol::MAXIMUM_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a message longer than this protocol sends",
            ));
        }
        let mut body = vec![0u8; length];
        if !read_exactly(reader, &mut body)? {
            return Ok(None);
        }
        // A kind this build has never heard of: its body has been read, so the
        // next message is where it was always going to be.
        if let Some(kind) = Kind::from_byte(header[0]) {
            return Ok(Some((kind, body)));
        }
    }
}

/// Whether the whole of `bytes` was read; `false` means the stream ended.
fn read_exactly<R: io::Read>(reader: &mut R, bytes: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) => return Ok(false),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

/// A key code, which serde does not know about: it is a number that names a
/// key, and that is how it crosses.
mod key_code {
    use evdev::KeyCode;
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde hands a field to its serializer by reference, whatever the field is"
    )]
    pub(super) fn serialize<S: Serializer>(
        code: &KeyCode,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(code.code())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<KeyCode, D::Error> {
        u16::deserialize(deserializer).map(KeyCode::new)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use super::{
        Capabilities, Hello, Input, Key, KeyCode, KeyKind, Pointer, Show, ToClient, ToServer,
        VERSION, WindowId, read_for_pane, read_from, write_for_pane, write_to,
    };

    fn capabilities() -> Capabilities {
        Capabilities {
            cell: (10, 20),
            cells: (120, 40),
            pixels: (1200, 800),
            terminal: Some("kitty(0.48.2)".to_owned()),
            graphics: true,
            keyboard: true,
            pixel_mouse: false,
            shared_memory: true,
        }
    }

    /// Write a message and read it back through the wire it would cross.
    fn round_trip(message: ToServer) {
        let mut bytes = Vec::new();
        write_to(&mut bytes, &message).expect("writing to a Vec cannot fail");
        let read = read_from(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(read, Some(message));
    }

    fn round_trip_client(message: ToClient) {
        let mut bytes = Vec::new();
        write_for_pane(&mut bytes, &message).expect("writing to a Vec cannot fail");
        let read = read_for_pane(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(read, Some(message));
    }

    #[test]
    fn a_hello_survives_a_round_trip() {
        for show in [Show::Window(WindowId::new(7)), Show::Newest, Show::Focused] {
            round_trip(ToServer::Hello(Hello {
                version: VERSION,
                show,
                capabilities: capabilities(),
            }));
        }
        round_trip(ToServer::Hello(Hello {
            version: VERSION,
            show: Show::Newest,
            capabilities: Capabilities {
                terminal: None,
                ..capabilities()
            },
        }));
    }

    #[test]
    fn every_kind_of_input_survives_a_round_trip() {
        round_trip(ToServer::Input(Input::Key(Key {
            code: KeyCode::new(30),
            shift: true,
            modifiers: 0b1010,
            kind: KeyKind::Repeat,
            modifier: false,
        })));
        round_trip(ToServer::Input(Input::Key(Key {
            code: KeyCode::new(56),
            shift: false,
            modifiers: 0,
            kind: KeyKind::Release,
            modifier: true,
        })));
        for pointer in [
            Pointer::Motion {
                column: 3,
                row: 4000,
            },
            Pointer::Button {
                column: 1,
                row: 2,
                button: KeyCode::new(272),
                pressed: true,
            },
            Pointer::ScrollUp,
            Pointer::ScrollDown,
            Pointer::ScrollLeft,
            Pointer::ScrollRight,
        ] {
            round_trip(ToServer::Input(Input::Pointer(pointer)));
        }
        round_trip(ToServer::Input(Input::Paste("two\nlines\t".to_owned())));
        round_trip(ToServer::Input(Input::Focus(false)));
        round_trip(ToServer::Resized(Capabilities {
            cell: (7, 9),
            ..capabilities()
        }));
        round_trip(ToServer::Drawn);
        round_trip(ToServer::Bye);
    }

    #[test]
    fn what_the_pane_is_told_survives_a_round_trip() {
        round_trip_client(ToClient::Welcome);
        round_trip_client(ToClient::Bytes(vec![0x1b, b'_', b'G', 0xff]));
        round_trip_client(ToClient::Frame(vec![0x1b, b'P', 0xff]));
        round_trip_client(ToClient::Detached("another terminal".to_owned()));
    }

    #[test]
    fn a_frame_written_where_it_lies_is_the_frame_the_terminal_reads() {
        // The presenter writes frames out of the buffer it built them in, not
        // out of a message made for each one. The shortcut must still put a
        // frame on the wire, not something the terminal never acknowledges.
        let frame = b"\x1b[?2026h\x1b[7;3H\x1b_Ga=T,i=1;\x1b\\\x1b[?2026l";
        let mut bytes = Vec::new();
        ToClient::write_frame(&mut bytes, frame).expect("writing to a Vec cannot fail");

        let read = read_for_pane(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(read, Some(ToClient::Frame(frame.to_vec())));
    }

    #[test]
    fn escapes_written_where_they_lie_are_what_the_pane_reads() {
        let escapes = b"\x1b]2;a title\x07";
        let mut bytes = Vec::new();
        ToClient::write_bytes(&mut bytes, escapes).expect("writing to a Vec cannot fail");

        let read = read_for_pane(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(read, Some(ToClient::Bytes(escapes.to_vec())));
    }

    #[test]
    fn a_message_that_is_cut_short_is_not_a_message() {
        let mut bytes = Vec::new();
        write_to(
            &mut bytes,
            &ToServer::Input(Input::Key(Key {
                code: KeyCode::new(30),
                shift: false,
                modifiers: 0,
                kind: KeyKind::Press,
                modifier: false,
            })),
        )
        .unwrap();

        // A stream that ends in the middle of a message has said nothing: every
        // prefix reads as the end of the conversation, not as a message.
        for length in 0..bytes.len() {
            let read =
                read_from(&mut Cursor::new(&bytes[..length])).expect("a cut stream is quiet");
            assert_eq!(read, None, "a message of {length} bytes");
        }
        assert!(read_from(&mut Cursor::new(&bytes)).unwrap().is_some());
    }

    /// A message of a kind no build of this knows, as a newer one would send
    /// it: a kind, the length of its body, and the body.
    fn unknown(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![kind];
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_message_this_version_does_not_know_is_skipped() {
        let mut bytes = unknown(200, b"from a newer build");
        write_to(&mut bytes, &ToServer::Drawn).unwrap();

        let read = read_from(&mut Cursor::new(bytes)).expect("the rest is readable");
        assert_eq!(read, Some(ToServer::Drawn));

        // The same the other way: what a newer server sends a pane.
        let mut bytes = unknown(201, b"new!");
        write_for_pane(&mut bytes, &ToClient::Welcome).unwrap();

        let read = read_for_pane(&mut Cursor::new(bytes)).expect("the rest is readable");
        assert_eq!(read, Some(ToClient::Welcome));
    }

    #[test]
    fn a_body_that_is_not_what_it_says_is_refused() {
        // A kind the build knows, with a body the codec cannot read: that is a
        // peer that disagrees about the protocol, not a message.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[1, 3, 0, 0, 0]);
        bytes.extend_from_slice(b"\xff\xff\xff");
        let error = read_from(&mut Cursor::new(bytes)).expect_err("that is not a hello");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// A stream that hands over at most `piece` bytes per read. A socket is
    /// allowed to do this to a reader.
    struct Piecemeal {
        bytes: Vec<u8>,
        at: usize,
        piece: usize,
    }

    impl io::Read for Piecemeal {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = out.len().min(self.piece).min(self.bytes.len() - self.at);
            out[..count].copy_from_slice(&self.bytes[self.at..self.at + count]);
            self.at += count;
            Ok(count)
        }
    }

    #[test]
    fn a_stream_that_arrives_in_pieces_still_reads() {
        let mut bytes = Vec::new();
        write_to(
            &mut bytes,
            &ToServer::Hello(Hello {
                version: VERSION,
                show: Show::Window(WindowId::new(3)),
                capabilities: capabilities(),
            }),
        )
        .unwrap();
        write_to(&mut bytes, &ToServer::Input(Input::Focus(false))).unwrap();

        for piece in 1..=bytes.len() {
            let mut stream = Piecemeal {
                bytes: bytes.clone(),
                at: 0,
                piece,
            };
            let read = read_from(&mut stream).expect("it is a message");
            assert_eq!(
                read,
                Some(ToServer::Hello(Hello {
                    version: VERSION,
                    show: Show::Window(WindowId::new(3)),
                    capabilities: capabilities(),
                })),
                "a piece of {piece}"
            );
            let read = read_from(&mut stream).expect("it is a message");
            assert_eq!(read, Some(ToServer::Input(Input::Focus(false))));
            assert_eq!(read_from(&mut stream).unwrap(), None);
        }
    }
}
