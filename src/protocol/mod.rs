//! What meowland's parts say to each other, and the names they say it about.
//!
//! [`pane`] is the protocol between the server and one terminal pane, spoken
//! over a unix socket. [`control`] is what the command line says to a running
//! server.

use std::io;

use bincode::config::{Configuration, Limit, LittleEndian, Varint};
use nutype::nutype;
use serde::{Serialize, de::DeserializeOwned};

pub mod control;
pub mod pane;

/// The most one message may carry, which is also the most either side will
/// allocate for one.
pub const MAXIMUM_MESSAGE: usize = 64 * 1024 * 1024;

/// The wire format of both protocols: bincode in its smallest form, bounded so
/// that a peer cannot make this side allocate without limit.
const CODEC: Configuration<LittleEndian, Varint, Limit<MAXIMUM_MESSAGE>> =
    bincode::config::standard().with_limit::<MAXIMUM_MESSAGE>();

/// A message, as the bytes that carry it.
pub fn encode<T: Serialize + ?Sized>(message: &T) -> io::Result<Vec<u8>> {
    encoded(bincode::serde::encode_to_vec(message, CODEC))
}

/// The message those bytes carry.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    decoded(bincode::serde::decode_from_slice(bytes, CODEC)).map(|(message, _)| message)
}

/// What the codec made of a write.
///
/// bincode reports the io errors it meets as its own, and those are the ones a
/// caller acts on: a socket that timed out is still a socket that timed out,
/// and only a message that cannot be read is the peer's fault.
fn encoded<T>(result: Result<T, bincode::error::EncodeError>) -> io::Result<T> {
    result.map_err(|error| match error {
        bincode::error::EncodeError::Io { inner, .. } => inner,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    })
}

/// What the codec made of a read, with the same care for io errors.
fn decoded<T>(result: Result<T, bincode::error::DecodeError>) -> io::Result<T> {
    result.map_err(|error| match error {
        bincode::error::DecodeError::Io { inner, .. } => inner,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    })
}

/// The stable ID assigned to a Wayland toplevel by the compositor.
#[nutype(
    const_fn,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        FromStr,
        Serialize,
        Deserialize
    )
)]
pub struct WindowId(u64);

/// The ID assigned to one attached terminal pane.
#[nutype(const_fn, derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display))]
pub struct PaneId(u64);

/// The version of the pane protocol spoken on a connection.
#[nutype(
    const_fn,
    derive(Debug, Clone, Copy, PartialEq, Eq, Display, Serialize, Deserialize)
)]
pub struct ProtocolVersion(u32);
