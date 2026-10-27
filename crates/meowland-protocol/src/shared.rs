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
}

#[derive(Deserialize, Serialize)]
struct Packet {
    width: u32,
    height: u32,
    token: [u8; 16],
}

impl SharedFrame {
    /// Full dimensions of the tightly packed RGB object.
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    /// POSIX object name restricted to Meowland's immutable frame namespace.
    pub fn name(&self) -> String {
        use std::fmt::Write as _;
        let mut name = String::with_capacity(48);
        name.push_str("/meowland-frame-");
        for byte in self.token {
            write!(name, "{byte:02x}").expect("writing to String cannot fail");
        }
        name
    }

    /// Open a published object and validate its complete RGB length.
    ///
    /// # Errors
    /// Returns an error if the object is missing, inaccessible, or incorrectly
    /// sized.
    pub fn open(&self) -> io::Result<File> {
        let file = File::from(shm::open(
            self.name().as_str(),
            shm::OFlags::RDONLY,
            Mode::empty(),
        )?);
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
        match shm::open(self.name().as_str(), shm::OFlags::RDONLY, Mode::empty()) {
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
        match shm::unlink(self.name().as_str()) {
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
        Ok(Self {
            size,
            token: packet.token,
        })
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
        let frame = SharedFrame { size, token };
        let file = shm::open(
            frame.name().as_str(),
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
