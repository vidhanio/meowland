//! The stream between a terminal and the server that draws on it.
//!
//! A server outlives the terminal it is shown on: it holds the compositor, the
//! windows and the clients, and it draws on whichever terminal is attached to
//! it at the moment. Attaching is what this module carries - the escapes the
//! terminal has to write, what the user did, and the two things said about the
//! attachment itself: hello, and detached.
//!
//! Every message is a one-byte tag, a length and a payload, in both directions
//! and on a stream that stays open for as long as the terminal is attached. The
//! length is what makes adding a message safe: a reader that does not know a
//! tag skips it and reads the next one, rather than losing the stream.
//!
//! Nothing here knows what a terminal or a compositor is beyond the shapes it
//! carries - a [`Capabilities`] is what the terminal side probed, and an
//! [`Input`] is what the user did, already in the terms the compositor counts
//! in.

use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
};

use crate::{control, tty::Capabilities};

/// Listen for terminals that want to show the server.
///
/// The guard removes the socket file when the server ends; a file left by a
/// server that was killed is taken over rather than refused
/// (`control::Socket`).
pub fn listen() -> Result<(control::Socket, UnixListener), control::Error> {
    control::Socket::bind(control::DISPLAY_SOCKET)
}

/// Connect to a server to show it.
pub fn connect() -> Result<UnixStream, control::Error> {
    control::connect(control::DISPLAY_SOCKET)
}

/// What both ends of the stream speak.
///
/// A server outlives upgrades, so the two ends can be different builds: a
/// client that says another version is turned away rather than understood
/// wrongly.
pub const VERSION: u32 = 2;

/// Which window a pane is asking to be shown.
///
/// Panes are independent: the server draws the window each one asks for, and
/// two of them may be showing the same window or one each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// The window the server has focused, resolved when the pane attaches.
    Focused,
    /// The newest window, from here on: what was just started is what is
    /// looked at.
    Newest,
    /// This window, by the ID the server gave it.
    Window(u64),
}

/// What a terminal tells the server it is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToServer {
    /// Attach this pane to the server, showing what [`Show`] asks for.
    Hello {
        version: u32,
        show: Show,
        capabilities: Capabilities,
    },
    /// The terminal was resized, and this is what it can do now.
    Resized(Capabilities),
    /// Something the user did.
    Input(Input),
    /// The terminal has written the last frame, so the next one may be
    /// composed.
    Drawn,
    /// The terminal is leaving on purpose.
    Bye,
}

/// What the server tells the terminal it is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToClient {
    /// The server is drawn on this terminal from here on.
    Welcome,
    /// Escapes for the terminal, to be written exactly as they are.
    Bytes(Vec<u8>),
    /// A frame, which is written and then acknowledged ([`ToServer::Drawn`]).
    ///
    /// Escapes and frames travel the same way and are written in the order they
    /// arrive; only a frame says something back, because only a frame is worth
    /// waiting for: a server with one frame in flight is a server whose next
    /// frame is the newest one.
    Frame(Vec<u8>),
    /// This terminal is not the one the server is drawn on any more, and why.
    Detached(String),
}

/// Something the user did at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Pointer(Pointer),
    /// Pasted text, which is not typed and has no keys of its own.
    Paste(String),
    /// Whether the terminal window is the one with the user's attention.
    Focus(bool),
}

/// A key, already reduced to what the compositor counts in.
///
/// The terminal side does this because it is the side that has the key codes:
/// everything from this point on is evdev, which is what the compositor
/// forwards and what key bindings are written against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: u32,
    /// Whether the symbol this key stands for needs shift, which the terminal
    /// side knows and the key code does not.
    pub shift: bool,
    /// The modifier state the terminal reported, as `KeyModifiers`' own bits.
    pub modifiers: u8,
    pub kind: KeyKind,
    /// Whether this key *is* a modifier, whose press and release are state for
    /// the client rather than keystrokes.
    pub modifier: bool,
}

/// What the terminal said the key was doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Press,
    Repeat,
    Release,
}

/// Something the pointer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Motion {
        column: u16,
        row: u16,
    },
    Button {
        column: u16,
        row: u16,
        /// The evdev button code, which is what the compositor forwards.
        button: u32,
        pressed: bool,
    },
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// One message, as it goes on the wire.
type Message = (u8, Vec<u8>);

/// Tags, one per variant, in both directions.
mod tag {
    pub const HELLO: u8 = 1;
    pub const INPUT: u8 = 2;
    pub const DRAWN: u8 = 3;
    pub const BYE: u8 = 4;
    pub const RESIZED: u8 = 5;

    pub const WELCOME: u8 = 6;
    pub const BYTES: u8 = 7;
    pub const DETACHED: u8 = 8;

    pub const KEY: u8 = 9;
    pub const POINTER: u8 = 10;
    pub const PASTE: u8 = 11;
    pub const FOCUS: u8 = 12;

    /// Frames this one acknowledges by writing them; the rest need no answer.
    pub const FRAME: u8 = 13;
}

/// A message as it is written.
pub fn encode(message: &ToServer) -> Message {
    match message {
        ToServer::Hello {
            version,
            show,
            capabilities,
        } => {
            let mut payload = Vec::new();
            payload.extend_from_slice(&version.to_le_bytes());
            put_show(&mut payload, *show);
            put_capabilities(&mut payload, capabilities);
            (tag::HELLO, payload)
        }
        ToServer::Resized(capabilities) => {
            let mut payload = Vec::new();
            put_capabilities(&mut payload, capabilities);
            (tag::RESIZED, payload)
        }
        ToServer::Input(input) => input_message(input),
        ToServer::Drawn => (tag::DRAWN, Vec::new()),
        ToServer::Bye => (tag::BYE, Vec::new()),
    }
}

/// A message as it is written.
pub fn encode_client(message: &ToClient) -> Message {
    match message {
        ToClient::Welcome => (tag::WELCOME, Vec::new()),
        // The escapes are the payload: wrapping them again would only cost a
        // copy of the largest thing this stream carries.
        ToClient::Bytes(bytes) => (tag::BYTES, bytes.clone()),
        ToClient::Frame(bytes) => (tag::FRAME, bytes.clone()),
        ToClient::Detached(reason) => (tag::DETACHED, reason.as_bytes().to_vec()),
    }
}

/// Read a message, or `None` when it is not one this version knows.
///
/// An unknown tag is not an error: it is what an upgrade looks like from the
/// older end, and the length is there so that the stream can carry on.
pub fn decode(tag: u8, payload: &[u8]) -> Option<ToServer> {
    let mut read = Reader::new(payload);
    Some(match tag {
        tag::HELLO => ToServer::Hello {
            version: read.u32()?,
            show: read.show()?,
            capabilities: read.capabilities()?,
        },
        tag::RESIZED => ToServer::Resized(read.capabilities()?),
        tag::INPUT => ToServer::Input(input_from(&mut read)?),
        tag::DRAWN => ToServer::Drawn,
        tag::BYE => ToServer::Bye,
        _ => return None,
    })
}

/// Read a message, or `None` when it is not one this version knows.
pub fn decode_client(tag: u8, payload: &[u8]) -> Option<ToClient> {
    Some(match tag {
        tag::WELCOME => ToClient::Welcome,
        tag::BYTES => ToClient::Bytes(payload.to_vec()),
        tag::FRAME => ToClient::Frame(payload.to_vec()),
        // The reason is the payload, like the escapes above.
        tag::DETACHED => ToClient::Detached(String::from_utf8(payload.to_vec()).ok()?),
        _ => return None,
    })
}

fn input_message(input: &Input) -> Message {
    let mut payload = Vec::new();
    match input {
        Input::Key(key) => {
            payload.push(tag::KEY);
            payload.extend_from_slice(&key.code.to_le_bytes());
            payload.push(u8::from(key.shift));
            payload.push(key.modifiers);
            payload.push(match key.kind {
                KeyKind::Press => 0,
                KeyKind::Repeat => 1,
                KeyKind::Release => 2,
            });
            payload.push(u8::from(key.modifier));
        }
        Input::Pointer(pointer) => {
            payload.push(tag::POINTER);
            match pointer {
                Pointer::Motion { column, row } => {
                    payload.push(0);
                    payload.extend_from_slice(&column.to_le_bytes());
                    payload.extend_from_slice(&row.to_le_bytes());
                }
                Pointer::Button {
                    column,
                    row,
                    button,
                    pressed,
                } => {
                    payload.push(1);
                    payload.extend_from_slice(&column.to_le_bytes());
                    payload.extend_from_slice(&row.to_le_bytes());
                    payload.extend_from_slice(&button.to_le_bytes());
                    payload.push(u8::from(*pressed));
                }
                Pointer::ScrollUp => payload.push(2),
                Pointer::ScrollDown => payload.push(3),
                Pointer::ScrollLeft => payload.push(4),
                Pointer::ScrollRight => payload.push(5),
            }
        }
        Input::Paste(text) => {
            payload.push(tag::PASTE);
            payload.extend_from_slice(&(text.len() as u32).to_le_bytes());
            payload.extend_from_slice(text.as_bytes());
        }
        Input::Focus(focused) => {
            payload.push(tag::FOCUS);
            payload.push(u8::from(*focused));
        }
    }
    (tag::INPUT, payload)
}

fn input_from(read: &mut Reader<'_>) -> Option<Input> {
    Some(match read.u8()? {
        tag::KEY => Input::Key(Key {
            code: read.u32()?,
            shift: read.flag()?,
            modifiers: read.u8()?,
            kind: match read.u8()? {
                0 => KeyKind::Press,
                1 => KeyKind::Repeat,
                _ => KeyKind::Release,
            },
            modifier: read.flag()?,
        }),
        tag::POINTER => match read.u8()? {
            0 => Input::Pointer(Pointer::Motion {
                column: read.u16()?,
                row: read.u16()?,
            }),
            1 => Input::Pointer(Pointer::Button {
                column: read.u16()?,
                row: read.u16()?,
                button: read.u32()?,
                pressed: read.flag()?,
            }),
            2 => Input::Pointer(Pointer::ScrollUp),
            3 => Input::Pointer(Pointer::ScrollDown),
            4 => Input::Pointer(Pointer::ScrollLeft),
            _ => Input::Pointer(Pointer::ScrollRight),
        },
        tag::PASTE => Input::Paste(read.string()?),
        tag::FOCUS => Input::Focus(read.flag()?),
        _ => return None,
    })
}

/// The kind byte each [`Show`] carries, and the window that goes with one of
/// them.
mod show {
    pub const FOCUSED: u8 = 0;
    pub const NEWEST: u8 = 1;
    pub const WINDOW: u8 = 2;
}

fn put_show(payload: &mut Vec<u8>, show: Show) {
    match show {
        Show::Focused => payload.push(show::FOCUSED),
        Show::Newest => payload.push(show::NEWEST),
        Show::Window(id) => {
            payload.push(show::WINDOW);
            payload.extend_from_slice(&id.to_le_bytes());
        }
    }
}

fn put_capabilities(payload: &mut Vec<u8>, capabilities: &Capabilities) {
    for value in [
        capabilities.cell.0,
        capabilities.cell.1,
        capabilities.cells.0,
        capabilities.cells.1,
        capabilities.pixels.0,
        capabilities.pixels.1,
    ] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    match &capabilities.terminal {
        Some(name) => {
            payload.push(1);
            payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
        }
        None => payload.push(0),
    }
    for value in [
        capabilities.graphics,
        capabilities.keyboard,
        capabilities.pixel_mouse,
        capabilities.shared_memory,
    ] {
        payload.push(u8::from(value));
    }
}

/// Reads a payload field by field, refusing to read past its end.
#[derive(Debug)]
struct Reader<'a> {
    payload: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    const fn new(payload: &'a [u8]) -> Self {
        Self { payload, at: 0 }
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let taken = self.payload.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Which window a pane asked to be shown.
    fn show(&mut self) -> Option<Show> {
        Some(match self.u8()? {
            show::FOCUSED => Show::Focused,
            show::NEWEST => Show::Newest,
            show::WINDOW => {
                let bytes = self.take(8)?;
                let mut value = [0u8; 8];
                value.copy_from_slice(bytes);
                Show::Window(u64::from_le_bytes(value))
            }
            _ => return None,
        })
    }

    fn string(&mut self) -> Option<String> {
        let length = self.u32()? as usize;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    fn capabilities(&mut self) -> Option<Capabilities> {
        Some(Capabilities {
            cell: (self.u32()?, self.u32()?),
            cells: (self.u32()?, self.u32()?),
            pixels: (self.u32()?, self.u32()?),
            terminal: if self.flag()? {
                Some(self.string()?)
            } else {
                None
            },
            graphics: self.flag()?,
            keyboard: self.flag()?,
            pixel_mouse: self.flag()?,
            shared_memory: self.flag()?,
        })
    }
}

/// The largest message this stream carries, which bounds what a reader will
/// believe a length to be before it decides the stream is not this protocol.
const MAXIMUM_MESSAGE: usize = 64 * 1024 * 1024;

/// Write one message and flush it.
pub fn write_to<W: io::Write>(writer: &mut W, message: (u8, Vec<u8>)) -> io::Result<()> {
    let (tag, payload) = message;
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Read one message, or `None` when the other end has gone.
pub fn read_from<R: io::Read>(reader: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    if !read_exactly(reader, &mut header)? {
        return Ok(None);
    }
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAXIMUM_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a message longer than this protocol sends",
        ));
    }
    let mut payload = vec![0u8; length];
    if !read_exactly(reader, &mut payload)? {
        return Ok(None);
    }
    Ok(Some((header[0], payload)))
}

/// Fill `bytes`, or say that the stream ended first.
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

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use super::{
        Capabilities, Input, Key, KeyKind, Pointer, Show, ToClient, ToServer, VERSION, decode,
        decode_client, encode, encode_client, read_from, write_to,
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

    fn round_trip(message: ToServer) {
        let (tag, payload) = encode(&message);
        assert_eq!(decode(tag, &payload), Some(message));
    }

    fn round_trip_client(message: ToClient) {
        let (tag, payload) = encode_client(&message);
        assert_eq!(decode_client(tag, &payload), Some(message));
    }

    #[test]
    fn a_hello_survives_a_round_trip() {
        round_trip(ToServer::Hello {
            version: VERSION,
            show: Show::Window(7),
            capabilities: capabilities(),
        });
        round_trip(ToServer::Hello {
            version: VERSION,
            show: Show::Newest,
            capabilities: Capabilities {
                terminal: None,
                ..capabilities()
            },
        });
        round_trip(ToServer::Hello {
            version: VERSION,
            show: Show::Focused,
            capabilities: capabilities(),
        });
    }

    #[test]
    fn every_kind_of_input_survives_a_round_trip() {
        round_trip(ToServer::Input(Input::Key(Key {
            code: 30,
            shift: true,
            modifiers: 0b1010,
            kind: KeyKind::Repeat,
            modifier: false,
        })));
        round_trip(ToServer::Input(Input::Key(Key {
            code: 56,
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
                button: 272,
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
    fn what_the_client_is_told_survives_a_round_trip() {
        round_trip_client(ToClient::Welcome);
        round_trip_client(ToClient::Bytes(vec![0x1b, b'_', b'G', 0xff]));
        round_trip_client(ToClient::Frame(vec![0x1b, b'P', 0xff]));
        round_trip_client(ToClient::Detached("another terminal".to_owned()));
    }

    #[test]
    fn a_message_that_is_cut_short_is_not_a_message() {
        let (tag, payload) = encode(&ToServer::Input(Input::Key(Key {
            code: 30,
            shift: false,
            modifiers: 0,
            kind: KeyKind::Press,
            modifier: false,
        })));
        let cut = payload.len() - 1;
        assert_eq!(decode(tag, &payload[..cut]), None);
        // Every prefix, not just the last byte: a field boundary is where a
        // reader would otherwise read the next field out of the one after it.
        for length in 0..payload.len() {
            assert_eq!(decode(tag, &payload[..length]), None, "{length}");
        }
    }

    #[test]
    fn a_message_this_version_does_not_know_is_skipped() {
        let mut bytes = Vec::new();
        {
            let mut writer = std::io::Cursor::new(&mut bytes);
            write_to(&mut writer, (200, b"from a newer build".to_vec())).unwrap();
            write_to(&mut writer, encode(&ToServer::Drawn)).unwrap();
        }
        let mut stream = Cursor::new(bytes);
        let (tag, payload) = read_from(&mut stream).unwrap().expect("a message");
        assert_eq!(decode(tag, &payload), None);
        let (tag, payload) = read_from(&mut stream).unwrap().expect("a message");
        assert_eq!(decode(tag, &payload), Some(ToServer::Drawn));
        assert_eq!(read_from(&mut stream).unwrap(), None);
    }

    /// A stream that hands over at most `piece` bytes per read, which is what a
    /// socket is allowed to do to a reader.
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
        {
            let mut writer = Cursor::new(&mut bytes);
            write_to(
                &mut writer,
                encode(&ToServer::Hello {
                    version: VERSION,
                    show: Show::Window(3),
                    capabilities: capabilities(),
                }),
            )
            .unwrap();
            write_to(&mut writer, encode(&ToServer::Input(Input::Focus(false)))).unwrap();
        }
        for piece in 1..=bytes.len() {
            let mut stream = Piecemeal {
                bytes: bytes.clone(),
                at: 0,
                piece,
            };
            let (tag, payload) = read_from(&mut stream).unwrap().expect("the hello");
            assert_eq!(
                decode(tag, &payload),
                Some(ToServer::Hello {
                    version: VERSION,
                    show: Show::Window(3),
                    capabilities: capabilities(),
                }),
                "a piece of {piece}"
            );
            let (tag, payload) = read_from(&mut stream).unwrap().expect("the focus");
            assert_eq!(
                decode(tag, &payload),
                Some(ToServer::Input(Input::Focus(false)))
            );
            assert_eq!(read_from(&mut stream).unwrap(), None);
        }
    }
}
