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

#[cfg(test)]
mod tests {
    use serde_bytes::ByteBuf;

    use super::*;

    fn frame_size(width: u32, height: u32) -> FrameSize {
        FrameSize::new(width, height).expect("valid test dimensions")
    }

    fn encode_packet(
        width: u32,
        height: u32,
        x: u32,
        y: u32,
        band_width: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        postcard::to_allocvec(&FramePacket {
            width,
            height,
            x,
            y,
            band_width,
            payload: ByteBuf::from(payload.to_vec()),
        })
        .expect("frame packet serializes")
    }

    #[test]
    fn frame_band_accepts_whole_and_partial_rectangles() {
        let size = frame_size(4, 3);
        let whole = FrameBand::new(size, 0, 0, 4, vec![0; 4 * 3 * 3]).expect("whole band");
        assert_eq!(whole.size(), size);
        assert_eq!((whole.x(), whole.y()), (0, 0));
        assert_eq!((whole.width(), whole.height()), (4, 3));
        assert!(whole.is_whole());

        let partial = FrameBand::new(size, 1, 1, 2, vec![0; 2 * 2 * 3]).expect("partial band");
        assert_eq!(partial.size(), size);
        assert_eq!((partial.x(), partial.y()), (1, 1));
        assert_eq!((partial.width(), partial.height()), (2, 2));
        assert!(!partial.is_whole());
        assert_eq!(partial.rgb(), &[0; 2 * 2 * 3]);

        let edge = FrameBand::new(size, 3, 2, 1, vec![0; 3]).expect("edge band");
        assert_eq!((edge.width(), edge.height()), (1, 1));
        assert!(!edge.is_whole());
    }

    #[test]
    fn frame_band_rejects_invalid_rectangles() {
        let size = frame_size(4, 3);
        let cases: &[(&str, u32, u32, u32, Vec<u8>)] = &[
            ("zero width", 0, 0, 0, vec![]),
            ("x at right edge", 4, 0, 1, vec![0; 3]),
            ("y at bottom edge", 0, 3, 1, vec![0; 3]),
            ("empty payload", 0, 0, 1, vec![]),
            ("partial row", 0, 0, 2, vec![0; 11]),
            ("x overflow", u32::MAX, 0, 1, vec![0; 3]),
            ("y overflow", 0, u32::MAX, 1, vec![0; 3]),
        ];
        for (name, x, y, width, rgb) in cases {
            assert!(
                FrameBand::new(size, *x, *y, *width, rgb.clone()).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn frame_band_deserializes_valid_packets() {
        let size = frame_size(4, 3);
        let band = FrameBand::new(size, 1, 0, 2, vec![1; 2 * 2 * 3]).expect("valid band");
        let bytes = postcard::to_allocvec(&band).expect("band serializes");
        let decoded: FrameBand = postcard::from_bytes(&bytes).expect("band deserializes");
        assert_eq!(decoded.size(), size);
        assert_eq!((decoded.x(), decoded.y()), (1, 0));
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        assert_eq!(decoded.rgb(), band.rgb());
    }

    #[test]
    fn frame_band_rejects_invalid_packets() {
        struct Case<'a> {
            name: &'a str,
            width: u32,
            height: u32,
            x: u32,
            y: u32,
            band_width: u32,
            payload: &'a [u8],
        }

        let cases = [
            Case {
                name: "zero frame width",
                width: 0,
                height: 3,
                x: 0,
                y: 0,
                band_width: 1,
                payload: &[0; 3],
            },
            Case {
                name: "oversized frame width",
                width: 8193,
                height: 3,
                x: 0,
                y: 0,
                band_width: 1,
                payload: &[0; 3],
            },
            Case {
                name: "zero band width",
                width: 4,
                height: 3,
                x: 0,
                y: 0,
                band_width: 0,
                payload: &[],
            },
            Case {
                name: "partial rows",
                width: 4,
                height: 3,
                x: 0,
                y: 0,
                band_width: 2,
                payload: &[0; 11],
            },
            Case {
                name: "x overflow",
                width: 4,
                height: 3,
                x: u32::MAX,
                y: 0,
                band_width: 1,
                payload: &[0; 3],
            },
            Case {
                name: "y overflow",
                width: 4,
                height: 3,
                x: 0,
                y: u32::MAX,
                band_width: 1,
                payload: &[0; 3],
            },
        ];
        for case in cases {
            let bytes = encode_packet(
                case.width,
                case.height,
                case.x,
                case.y,
                case.band_width,
                case.payload,
            );
            let error = postcard::from_bytes::<FrameBand>(&bytes).expect_err(case.name);
            assert_eq!(error, postcard::Error::SerdeDeCustom, "{}", case.name);
        }
    }
}
