//! Pixel content of every workload frame.
//!
//! The workload paints its buffers from [`Pattern::surface_pixel`], and the
//! endpoint identifies frames with [`identify`] and checks them with
//! [`verify`], so the producer and the validator cannot drift apart.

use std::{error::Error, fmt, str::FromStr};

/// App ID the workload gives its toplevel.
pub const APP_ID: &str = "meowland-workload";

/// Pixels at the start of row zero that carry a frame's [`marker`].
pub const MARKER_PIXELS: usize = 16;

/// A rectangle in pixels: `(x, y, width, height)`.
pub type Rect = (usize, usize, usize, usize);

/// The sparse pattern's patch occupies rows `100..120`, starting at column 80
/// on even frames and 100 on odd ones.
const PATCH_TOP: usize = 100;
const PATCH_SIZE: usize = 20;
const PATCH_LEFT: [usize; 2] = [80, 100];

/// How a workload frame's pixels depend on its ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Every pixel changes with each frame.
    Full,
    /// A uniform background, the marker, and a 20×20 patch alternating
    /// between two positions.
    Sparse,
    /// [`Pattern::Full`] content, with odd frames attaching a buffer half the
    /// size in each dimension.
    Resize,
}

impl Pattern {
    /// Name used on the workload command line.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Sparse => "sparse",
            Self::Resize => "resize",
        }
    }

    /// Surface size of `frame` for a workload configured as `width`×`height`.
    pub const fn surface(self, width: usize, height: usize, frame: u64) -> (usize, usize) {
        if matches!(self, Self::Resize) && frame & 1 == 1 {
            (width / 2, height / 2)
        } else {
            (width, height)
        }
    }

    /// Regions that can differ between any two frames, or `None` when every
    /// pixel can.
    pub const fn damage(self) -> Option<[Rect; 2]> {
        match self {
            Self::Sparse => Some([
                (0, 0, MARKER_PIXELS, 1),
                (
                    PATCH_LEFT[0],
                    PATCH_TOP,
                    PATCH_LEFT[1] + PATCH_SIZE - PATCH_LEFT[0],
                    PATCH_SIZE,
                ),
            ]),
            Self::Full | Self::Resize => None,
        }
    }

    /// RGB of pixel `(x, y)` of `frame`'s surface, which is `width` pixels
    /// wide.
    ///
    /// # Panics
    /// Panics if the pixel index does not fit in `u64`.
    pub fn surface_pixel(self, x: usize, y: usize, width: usize, frame: u64) -> [u8; 3] {
        if y == 0 && x < MARKER_PIXELS {
            let value = marker(frame)[x];
            return [value; 3];
        }
        match self {
            Self::Full | Self::Resize => {
                let index = u64::try_from(y * width + x).expect("pixel index fits u64");
                let [blue, green, red, ..] = (index.wrapping_mul(0x9e37_79b9)
                    ^ frame.wrapping_mul(0x0001_0101))
                .to_le_bytes();
                [red, green, blue]
            }
            Self::Sparse => {
                let left = PATCH_LEFT[usize::from(frame & 1 == 1)];
                let inside = (PATCH_TOP..PATCH_TOP + PATCH_SIZE).contains(&y)
                    && (left..left + PATCH_SIZE).contains(&x);
                [if inside { 173 } else { 31 }; 3]
            }
        }
    }

    /// RGB a `width`×`height` pane presenting `frame` shows at `(x, y)`: the
    /// surface anchored top-left, black beyond it.
    pub fn pixel(self, x: usize, y: usize, width: usize, height: usize, frame: u64) -> [u8; 3] {
        let (surface_width, surface_height) = self.surface(width, height, frame);
        if x < surface_width && y < surface_height {
            self.surface_pixel(x, y, surface_width, frame)
        } else {
            [0; 3]
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl FromStr for Pattern {
    type Err = UnknownPattern;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        [Self::Full, Self::Sparse, Self::Resize]
            .into_iter()
            .find(|pattern| pattern.name() == name)
            .ok_or(UnknownPattern)
    }
}

/// A pattern name other than `full`, `sparse`, or `resize`.
#[derive(Clone, Copy, Debug)]
pub struct UnknownPattern;

impl fmt::Display for UnknownPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("pattern must be full, sparse, or resize")
    }
}

impl Error for UnknownPattern {}

/// Grey levels of the [`MARKER_PIXELS`] identifying `frame`: `MEOW`, the ID,
/// and a checksum, so a stale or torn row never passes for another frame.
pub fn marker(frame: u64) -> [u8; MARKER_PIXELS] {
    let mut bytes = [0; MARKER_PIXELS];
    bytes[..4].copy_from_slice(b"MEOW");
    bytes[4..12].copy_from_slice(&frame.to_le_bytes());
    bytes[12..].copy_from_slice(&(frame ^ (frame >> 32) ^ 0x91a7_485e).to_le_bytes()[..4]);
    bytes
}

/// The frame whose marker starts packed RGB `row`, if it carries a valid one.
pub fn identify(row: &[u8]) -> Option<u64> {
    let pixels = row.get(..MARKER_PIXELS * 3)?.as_chunks::<3>().0;
    let mut bytes = [0; MARKER_PIXELS];
    for (byte, &[red, green, blue]) in bytes.iter_mut().zip(pixels) {
        if red != green || red != blue {
            return None;
        }
        *byte = red;
    }
    let frame = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    (bytes == marker(frame)).then_some(frame)
}

/// Check content-sensitive probes of a packed RGB `width`×`height` image
/// against `frame`: both sides of the sparse patch, the corners, a diagonal,
/// and, for shrunken frames, the cleared area beyond the surface.
///
/// # Errors
/// Returns the first probe whose pixel differs.
pub fn verify(
    image: &[u8],
    width: usize,
    height: usize,
    pattern: Pattern,
    frame: u64,
) -> Result<(), Mismatch> {
    let fixed = [
        (20, 0),
        (85, 105),
        (105, 105),
        (125, 105),
        (0, height - 1),
        (width - 1, height - 1),
    ];
    let diagonal = (1..13).map(|step| (width * step / 13, height * step / 13));
    for (x, y) in fixed.into_iter().chain(diagonal) {
        let expected = pattern.pixel(x, y, width, height, frame);
        let at = (y * width + x) * 3;
        let actual = image
            .get(at..at + 3)
            .and_then(|pixel| pixel.try_into().ok());
        if actual != Some(expected) {
            return Err(Mismatch {
                frame,
                x,
                y,
                expected,
                actual,
            });
        }
    }
    Ok(())
}

/// A probe pixel that does not match its frame.
#[derive(Clone, Copy, Debug)]
pub struct Mismatch {
    /// Frame the image claims to show.
    pub frame: u64,
    /// Probe column.
    pub x: usize,
    /// Probe row.
    pub y: usize,
    /// RGB the pattern requires.
    pub expected: [u8; 3],
    /// RGB found, or `None` if the image is too short.
    pub actual: Option<[u8; 3]>,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "frame {}: RGB mismatch at {},{}: {:?} != expected {:?}",
            self.frame, self.x, self.y, self.actual, self.expected
        )
    }
}

impl Error for Mismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(pattern: Pattern, width: usize, height: usize, frame: u64) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| pattern.pixel(x, y, width, height, frame)))
            .collect()
    }

    /// A marker with any altered ID or checksum byte must not identify a
    /// frame, or a stale or torn image would be counted as fresh.
    #[test]
    fn identify_rejects_corrupted_markers() {
        let row: Vec<u8> = marker(0x1_0000_0002)
            .into_iter()
            .flat_map(|value| [value; 3])
            .collect();
        assert_eq!(identify(&row), Some(0x1_0000_0002));
        for pixel in 4..MARKER_PIXELS {
            let mut corrupt = row.clone();
            for channel in &mut corrupt[pixel * 3..pixel * 3 + 3] {
                *channel ^= 1;
            }
            assert_eq!(identify(&corrupt), None, "pixel {pixel}");
        }
    }

    /// Every pattern must change some probe between consecutive frames, or
    /// the endpoint could accept the previous frame's pixels.
    #[test]
    fn verify_rejects_the_previous_frame() {
        for pattern in [Pattern::Full, Pattern::Sparse, Pattern::Resize] {
            for frame in 1..4 {
                let mut image = render(pattern, 640, 360, frame - 1);
                image[..MARKER_PIXELS * 3]
                    .copy_from_slice(&render(pattern, 640, 360, frame)[..MARKER_PIXELS * 3]);
                assert!(
                    verify(&image, 640, 360, pattern, frame).is_err(),
                    "{pattern} frame {frame}"
                );
                let image = render(pattern, 640, 360, frame);
                verify(&image, 640, 360, pattern, frame).unwrap();
            }
        }
    }

    /// The sparse pattern's declared damage must cover every pixel that
    /// changes, or the workload would violate the buffer-damage contract.
    #[test]
    fn sparse_damage_covers_every_change() {
        let damage = Pattern::Sparse.damage().unwrap();
        for y in 0..240 {
            for x in 0..320 {
                let inside = damage.iter().any(|&(left, top, w, h)| {
                    (left..left + w).contains(&x) && (top..top + h).contains(&y)
                });
                let changes = (0..4).any(|frame| {
                    Pattern::Sparse.surface_pixel(x, y, 320, frame)
                        != Pattern::Sparse.surface_pixel(x, y, 320, frame + 1)
                });
                assert!(inside || !changes, "{x},{y}");
            }
        }
    }
}
