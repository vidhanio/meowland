//! The pane protocol: what a terminal pane and the server say to each other.
//!
//! One terminal holds one end of a socket to the server, and this is what
//! crosses it. Every message is bincode, as on the control socket. The two that
//! carry bytes the compositor and the terminal pass through -- an escape, and a
//! frame -- say they are byte strings, so a frame goes out of the buffer it was
//! built in and in through one read, rather than a write and a read per byte.
//!
//! A build that does not know a message cannot skip it, the way a kind byte
//! would let it: bump [`VERSION`] whenever these messages change.

use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
};

use evdev::KeyCode;
use serde::{Deserialize, Serialize};

use super::{decoded, encoded};
use crate::protocol::{ProtocolVersion, WindowId, control};

pub fn listen() -> Result<(control::Socket, UnixListener), crate::Error> {
    control::Socket::bind(control::DISPLAY_SOCKET)
}

pub fn connect() -> Result<UnixStream, crate::Error> {
    control::connect(control::DISPLAY_SOCKET)
}

/// The version of this protocol.
///
/// Bump it whenever these messages change: a build that does not know one of
/// them can only read on and let the connection end, so a pane and a server
/// that disagree are turned away rather than half-understood.
pub const VERSION: ProtocolVersion = ProtocolVersion::new(4);

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
    /// Whether the terminal reads frames out of shared memory, which keeps
    /// their pixels off the pty.
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
    Bytes(#[serde(with = "bytes")] Vec<u8>),
    /// Acknowledged with [`ToServer::Drawn`].
    Frame(#[serde(with = "bytes")] Vec<u8>),
    Detached(String),
}

/// What is written to a pane.
///
/// The same messages as [`ToClient`], with one difference: a frame or an escape
/// borrows the buffer it was built in, so writing one copies nothing and
/// allocates nothing. Keep these variants in step with [`ToClient`]: bincode
/// names them by their order.
#[derive(Debug, Serialize)]
enum Written<'a> {
    Welcome,
    Bytes(#[serde(with = "bytes")] &'a [u8]),
    Frame(#[serde(with = "bytes")] &'a [u8]),
    Detached(&'a str),
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

/// What a pane says to the server.
pub fn write<W: io::Write>(writer: &mut W, message: &ToServer) -> io::Result<()> {
    put(writer, message)
}

/// The next thing a pane said, or why the socket is no longer readable: a
/// message that cannot be read and a peer that has gone look the same here, and
/// both end the conversation.
pub fn read<R: io::Read, M: serde::de::DeserializeOwned>(reader: &mut R) -> io::Result<M> {
    decoded(bincode::serde::decode_from_std_read(
        reader,
        crate::protocol::CODEC,
    ))
}

/// Tell a pane it may show a window.
pub fn write_welcome<W: io::Write>(writer: &mut W) -> io::Result<()> {
    put(writer, &Written::Welcome)
}

/// Put escapes on the wire: the terminal's own bytes, written out of the buffer
/// they were built in.
pub fn write_bytes<W: io::Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    put(writer, &Written::Bytes(bytes))
}

/// Put a frame on the wire: the bytes the terminal takes, written out of the
/// buffer the presenter built them in.
pub fn write_frame<W: io::Write>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
    put(writer, &Written::Frame(frame))
}

/// Let go of a pane, and say why.
pub fn write_detached<W: io::Write>(writer: &mut W, reason: &str) -> io::Result<()> {
    put(writer, &Written::Detached(reason))
}

/// Write one message. Everything this protocol writes goes through here, so
/// there is one place that names the codec.
fn put<W: io::Write, M: Serialize + ?Sized>(writer: &mut W, message: &M) -> io::Result<()> {
    encoded(bincode::serde::encode_into_std_write(
        message,
        writer,
        crate::protocol::CODEC,
    ))
    .map(|_| ())
}

/// A byte string, which serde would otherwise treat as a sequence of numbers:
/// a write and a read for each byte of a frame, rather than one for all of it.
mod bytes {
    use std::fmt;

    use serde::{Deserializer, Serializer, de::Visitor};

    pub(super) fn serialize<S, B>(bytes: &B, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        B: AsRef<[u8]> + ?Sized,
    {
        serializer.serialize_bytes(bytes.as_ref())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        struct ByteString;

        impl Visitor<'_> for ByteString {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a byte string")
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Vec<u8>, E> {
                Ok(bytes.to_vec())
            }

            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(bytes)
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Vec<u8>, E> {
                Err(E::custom(format!("a byte string, not {text:?}")))
            }
        }

        deserializer.deserialize_byte_buf(ByteString)
    }
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
        VERSION, WindowId, read, write, write_bytes, write_detached, write_frame, write_welcome,
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

    /// What a pane says to the server, and what the server makes of it.
    fn round_trip(message: &ToServer) {
        let mut bytes = Vec::new();
        write(&mut bytes, message).expect("writing to a Vec cannot fail");
        let back = read::<_, ToServer>(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(&back, message);
    }

    #[test]
    fn a_hello_survives_a_round_trip() {
        for show in [Show::Window(WindowId::new(7)), Show::Newest, Show::Focused] {
            round_trip(&ToServer::Hello(Hello {
                version: VERSION,
                show,
                capabilities: capabilities(),
            }));
        }
        round_trip(&ToServer::Hello(Hello {
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
        round_trip(&ToServer::Input(Input::Key(Key {
            code: KeyCode::new(30),
            shift: true,
            modifiers: 0b1010,
            kind: KeyKind::Repeat,
            modifier: false,
        })));
        round_trip(&ToServer::Input(Input::Key(Key {
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
            round_trip(&ToServer::Input(Input::Pointer(pointer)));
        }
        round_trip(&ToServer::Input(Input::Paste("two\nlines\t".to_owned())));
        round_trip(&ToServer::Input(Input::Focus(false)));
        round_trip(&ToServer::Resized(Capabilities {
            cell: (7, 9),
            ..capabilities()
        }));
        round_trip(&ToServer::Drawn);
        round_trip(&ToServer::Bye);
    }

    #[test]
    fn what_the_pane_is_told_survives_a_round_trip() {
        // One connection's worth of messages, in the order a pane hears them.
        let frame = b"\x1b[?2026h\x1b_Ga=T\x1b\\\x1b[?2026l";
        let mut bytes = Vec::new();
        write_welcome(&mut bytes).expect("writing to a Vec cannot fail");
        write_bytes(&mut bytes, b"\x1b]2;a title\x07").expect("writing to a Vec cannot fail");
        write_frame(&mut bytes, frame).expect("writing to a Vec cannot fail");
        write_detached(&mut bytes, "another terminal").expect("writing to a Vec cannot fail");

        let mut stream = Cursor::new(bytes);
        assert_eq!(
            read::<_, ToClient>(&mut stream).expect("the welcome"),
            ToClient::Welcome
        );
        assert_eq!(
            read::<_, ToClient>(&mut stream).expect("the escapes"),
            ToClient::Bytes(b"\x1b]2;a title\x07".to_vec())
        );
        assert_eq!(
            read::<_, ToClient>(&mut stream).expect("the frame"),
            ToClient::Frame(frame.to_vec())
        );
        assert_eq!(
            read::<_, ToClient>(&mut stream).expect("the goodbye"),
            ToClient::Detached("another terminal".to_owned())
        );
    }

    #[test]
    fn a_frame_written_where_it_lies_is_the_frame_the_terminal_reads() {
        // The presenter writes frames out of the buffer it built them in, not
        // out of a message made for each one. The shortcut must still put a
        // frame on the wire, not something the terminal never acknowledges.
        let frame = b"\x1b[?2026h\x1b[7;3H\x1b_Ga=T,i=1;\x1b\\\x1b[?2026l";
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame).expect("writing to a Vec cannot fail");

        let read = read::<_, ToClient>(&mut Cursor::new(bytes)).expect("it is a message");
        assert_eq!(read, ToClient::Frame(frame.to_vec()));
    }

    #[test]
    fn a_message_that_is_cut_short_is_not_a_message() {
        let mut bytes = Vec::new();
        write(
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

        // A stream that ends in the middle of a message has said nothing: no
        // prefix of one is a message.
        for length in 0..bytes.len() {
            let message = read::<_, ToServer>(&mut Cursor::new(&bytes[..length]));
            assert!(message.is_err(), "a message of {length} bytes");
        }
        assert!(read::<_, ToServer>(&mut Cursor::new(&bytes)).is_ok());
    }

    #[test]
    fn a_message_this_build_does_not_know_ends_the_conversation() {
        // A message from a build that disagrees about the protocol cannot be
        // read, and there is nothing to skip it by: the connection ends, and
        // that is what the version check above is for.
        let error = read::<_, ToServer>(&mut Cursor::new([9u8])).expect_err("not a message");
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
        write(
            &mut bytes,
            &ToServer::Hello(Hello {
                version: VERSION,
                show: Show::Window(WindowId::new(3)),
                capabilities: capabilities(),
            }),
        )
        .unwrap();
        write(&mut bytes, &ToServer::Input(Input::Focus(false))).unwrap();

        for piece in 1..=bytes.len() {
            let mut stream = Piecemeal {
                bytes: bytes.clone(),
                at: 0,
                piece,
            };
            let message = read::<_, ToServer>(&mut stream).expect("it is a message");
            assert_eq!(
                message,
                ToServer::Hello(Hello {
                    version: VERSION,
                    show: Show::Window(WindowId::new(3)),
                    capabilities: capabilities(),
                }),
                "a piece of {piece}"
            );
            let message = read::<_, ToServer>(&mut stream).expect("it is a message");
            assert_eq!(message, ToServer::Input(Input::Focus(false)));
            assert!(
                read::<_, ToServer>(&mut stream).is_err(),
                "the stream is spent"
            );
        }
    }
}
