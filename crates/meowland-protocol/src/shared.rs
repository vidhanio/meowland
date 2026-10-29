//! Immutable shared-frame descriptors and producer-owned object cleanup.

use std::{fs::File, io};

use meowland_core::pixels::FrameSize;
use rustix::{fs::Mode, shm};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Pixel transport selected after probing the terminal's shared-memory support.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum FrameTransport {
    /// Send owned RGB bands through the pane socket.
    #[default]
    Inline,
    /// Publish immutable whole frames through named POSIX shared memory.
    Shared,
}

/// Validated dimensions and an unguessable name for an immutable RGB object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedFrame {
    size: FrameSize,
    token: [u8; 16],
    /// Preformatted object name, so opening or polling the object never
    /// allocates.
    name: [u8; NAME_LEN],
}

/// Object name prefix; the rest is the token in lowercase hex.
const NAME_PREFIX: &[u8] = b"/meowland-frame-";
/// Bytes in a shared-frame object name: the prefix plus 32 hex digits.
const NAME_LEN: usize = NAME_PREFIX.len() + 2 * 16;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

#[derive(Deserialize, Serialize)]
struct Packet {
    width: u32,
    height: u32,
    token: [u8; 16],
}

impl SharedFrame {
    /// Build a frame and its preformatted object name from a token.
    fn new(size: FrameSize, token: [u8; 16]) -> Self {
        let mut name = [0; NAME_LEN];
        name[..NAME_PREFIX.len()].copy_from_slice(NAME_PREFIX);
        for (index, byte) in token.into_iter().enumerate() {
            name[NAME_PREFIX.len() + 2 * index] = HEX_DIGITS[usize::from(byte >> 4)];
            name[NAME_PREFIX.len() + 2 * index + 1] = HEX_DIGITS[usize::from(byte & 0x0f)];
        }
        Self { size, token, name }
    }

    /// Full dimensions of the tightly packed RGB object.
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    /// POSIX object name restricted to Meowland's immutable frame namespace.
    ///
    /// # Panics
    /// Panics if the cached name is not ASCII hex, which construction and
    /// deserialization both guarantee.
    #[must_use]
    pub fn name(&self) -> &str {
        std::str::from_utf8(&self.name).expect("shared frame names are ASCII hex")
    }

    /// Open a published object and validate its complete RGB length.
    ///
    /// # Errors
    /// Returns an error if the object is missing, inaccessible, or incorrectly
    /// sized.
    pub fn open(&self) -> io::Result<File> {
        let file = File::from(shm::open(self.name(), shm::OFlags::RDONLY, Mode::empty())?);
        if file.metadata()?.len() != u64::try_from(self.size.rgb_len()).map_err(io::Error::other)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid shared frame length",
            ));
        }
        Ok(file)
    }

    /// Whether the terminal has not yet unlinked this object's name.
    ///
    /// An unlinked object can still have live readers; it must never be reused.
    ///
    /// # Errors
    /// Returns an error if checking the object's name fails for reasons other
    /// than absence.
    pub fn exists(&self) -> io::Result<bool> {
        match shm::open(self.name(), shm::OFlags::RDONLY, Mode::empty()) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Unlink the published name without invalidating existing file handles.
    ///
    /// # Errors
    /// Returns an error if unlinking fails for reasons other than absence.
    pub fn unlink(&self) -> io::Result<()> {
        match shm::unlink(self.name()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Serialize for SharedFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Packet {
            width: self.size.width(),
            height: self.size.height(),
            token: self.token,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SharedFrame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let packet = Packet::deserialize(deserializer)?;
        let size = FrameSize::new(packet.width, packet.height)
            .ok_or_else(|| D::Error::custom("invalid shared frame dimensions"))?;
        Ok(Self::new(size, packet.token))
    }
}

/// Producer cleanup retained until acknowledgement or pane detachment.
#[derive(Debug)]
#[must_use]
pub struct SharedObject {
    frame: SharedFrame,
}

impl SharedObject {
    /// Create a fresh object for sequential RGB writes, without mapping it.
    ///
    /// Publish the descriptor only after writing exactly one complete frame.
    /// Keep this guard until the consumer acknowledges or abandons the
    /// transfer.
    ///
    /// # Errors
    /// Returns an error if generating a random name or creating the object
    /// fails.
    pub fn create(size: FrameSize) -> io::Result<(Self, File)> {
        let mut token = [0; 16];
        getrandom::fill(&mut token).map_err(io::Error::other)?;
        let frame = SharedFrame::new(size, token);
        let file = shm::open(
            frame.name(),
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )?;
        Ok((Self { frame }, File::from(file)))
    }

    /// Descriptor to publish after completing the object's immutable contents.
    pub const fn frame(&self) -> &SharedFrame {
        &self.frame
    }
}

impl Drop for SharedObject {
    fn drop(&mut self) {
        let _ = self.frame.unlink();
    }
}
