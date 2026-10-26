//! Validated RGB bands and their explicit metadata/payload wire layout.

use meowland_core::pixels::FrameSize;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Nonempty, whole RGB rows within a validated full-frame size.
#[derive(Clone, Debug)]
pub struct FrameBand {
    size: FrameSize,
    y: u32,
    rgb: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
pub struct FramePacket<P> {
    width: u32,
    height: u32,
    y: u32,
    payload: P,
}

/// A Postcard byte string starts with its length, followed by its raw bytes.
#[cfg(feature = "async")]
pub type FrameHeader = FramePacket<usize>;

impl FrameBand {
    #[must_use]
    pub fn new(size: FrameSize, y: u32, rgb: Vec<u8>) -> Option<Self> {
        size.rgb_band(y, rgb.len())?;
        Some(Self { size, y, rgb })
    }

    #[must_use]
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    #[must_use]
    pub const fn y(&self) -> u32 {
        self.y
    }

    #[must_use]
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    #[must_use]
    pub fn into_parts(self) -> (FrameSize, u32, Vec<u8>) {
        (self.size, self.y, self.rgb)
    }

    #[cfg(feature = "async")]
    pub(super) const fn header(&self) -> FrameHeader {
        self.packet(self.rgb.len())
    }

    const fn packet<P>(&self, payload: P) -> FramePacket<P> {
        FramePacket {
            width: self.size.width(),
            height: self.size.height(),
            y: self.y,
            payload,
        }
    }
}

impl Serialize for FrameBand {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.packet(serde_bytes::Bytes::new(&self.rgb))
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FrameBand {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let packet = FramePacket::<serde_bytes::ByteBuf>::deserialize(deserializer)?;
        let size = FrameSize::new(packet.width, packet.height)
            .ok_or_else(|| D::Error::custom("invalid frame dimensions"))?;
        Self::new(size, packet.y, packet.payload.into_vec())
            .ok_or_else(|| D::Error::custom("invalid RGB row band"))
    }
}
