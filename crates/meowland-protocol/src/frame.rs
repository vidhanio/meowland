//! Validated RGB rectangles and their explicit metadata/payload wire layout.

use meowland_core::pixels::FrameSize;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Nonempty, whole RGB rows within a validated full-frame size.
#[derive(Clone, Debug)]
pub struct FrameBand {
    size: FrameSize,
    x: u32,
    y: u32,
    width: u32,
    rgb: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct FramePacket<P> {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    band_width: u32,
    payload: P,
}

/// A Postcard byte string starts with its length, followed by its raw bytes.
#[cfg(feature = "async")]
pub(crate) type FrameHeader = FramePacket<usize>;

impl FrameBand {
    /// Retain tightly packed RGB rows only when they form a nonempty
    /// rectangle of `width` pixels at `x`, `y` inside `size`.
    #[must_use]
    pub fn new(size: FrameSize, x: u32, y: u32, width: u32, rgb: Vec<u8>) -> Option<Self> {
        if width == 0 || x.checked_add(width)? > size.width() {
            return None;
        }
        let row_bytes = usize::try_from(width).ok()?.checked_mul(3)?;
        if rgb.is_empty() || !rgb.len().is_multiple_of(row_bytes) {
            return None;
        }
        let rows = u32::try_from(rgb.len() / row_bytes).ok()?;
        if y.checked_add(rows)? > size.height() {
            return None;
        }
        Some(Self {
            size,
            x,
            y,
            width,
            rgb,
        })
    }

    /// Full image dimensions, including pixels not present in this band.
    #[must_use]
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    /// Leftmost column replaced by this band.
    #[must_use]
    pub const fn x(&self) -> u32 {
        self.x
    }

    /// First row replaced by this band, counted from the top of the image.
    #[must_use]
    pub const fn y(&self) -> u32 {
        self.y
    }

    /// Columns replaced by this band, starting at [`FrameBand::x`].
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Rows replaced by this band, starting at [`FrameBand::y`].
    ///
    /// # Panics
    /// Panics if the validated payload is not a whole number of rows, which
    /// construction and deserialization both prevent.
    #[must_use]
    pub fn height(&self) -> u32 {
        let row_bytes = usize::try_from(self.width).expect("validated band width fits usize") * 3;
        u32::try_from(self.rgb.len() / row_bytes).expect("validated band height fits u32")
    }

    /// Whether this band replaces every pixel of its frame.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.x == 0
            && self.y == 0
            && self.width == self.size.width()
            && self.rgb.len() == self.size.rgb_len()
    }

    /// Tightly packed RGB bytes for the consecutive rows in this band.
    #[must_use]
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// Transfer pixel storage without copying.
    #[must_use]
    pub fn into_rgb(self) -> Vec<u8> {
        self.rgb
    }

    #[cfg(feature = "async")]
    pub(super) fn header(&self) -> FrameHeader {
        self.packet(self.rgb.len())
    }

    fn packet<P>(&self, payload: P) -> FramePacket<P> {
        FramePacket {
            width: self.size.width(),
            height: self.size.height(),
            x: self.x,
            y: self.y,
            band_width: self.width,
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
        Self::new(
            size,
            packet.x,
            packet.y,
            packet.band_width,
            packet.payload.into_vec(),
        )
        .ok_or_else(|| D::Error::custom("invalid RGB band"))
    }
}
