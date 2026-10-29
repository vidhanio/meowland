#[derive(Clone, Copy)]
pub(crate) enum Pattern {
    Full,
    Sparse,
    Resize,
    /// A narrow vertical strip away from the left edge.
    Tall,
    /// A wide, short strip near the top.
    Wide,
}

impl Pattern {
    /// Single damage rectangle for repeated updates, proportional to the frame.
    ///
    /// `Full`, `Sparse`, and `Resize` declare their own damage and return
    /// `None`. The benchmark harness derives its pixel expectations from the
    /// same geometry, so both sides must change together.
    pub(crate) const fn damage(
        self,
        width: usize,
        height: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        match self {
            Self::Tall => Some((width / 2, 0, 16, height * 5 / 9)),
            Self::Wide => Some((0, height * 5 / 18, width * 5 / 6, height / 18)),
            Self::Full | Self::Sparse | Self::Resize => None,
        }
    }

    /// Top-left pixel of the frame marker, kept inside [`Pattern::damage`].
    pub(crate) fn marker_origin(self, width: usize, height: usize) -> (usize, usize) {
        self.damage(width, height)
            .map_or((0, 0), |(x, y, ..)| (x, y))
    }
}

pub(crate) const APP_ID: &str = "meowland-workload";

pub(crate) fn marker(id: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(b"MEOW");
    bytes[4..12].copy_from_slice(&id.to_le_bytes());
    bytes[12..].copy_from_slice(&(id ^ (id >> 32) ^ 0x91a7_485e).to_le_bytes()[..4]);
    bytes
}

pub(crate) fn paint(pixels: &mut [u8], width: usize, height: usize, pattern: Pattern, id: u64) {
    match pattern.damage(width, height) {
        Some((x, y, w, h)) => paint_rectangle(pixels, width, id, (x, y, w, h)),
        None => paint_frame(pixels, width, pattern, id),
    }
    let (x, y) = pattern.marker_origin(width, height);
    let at = y * width + x;
    for (pixel, value) in pixels.as_chunks_mut::<4>().0[at..]
        .iter_mut()
        .zip(marker(id))
    {
        *pixel = [value, value, value, 255];
    }
}

fn hash(index: usize, id: u64) -> [u8; 4] {
    let value = u64::try_from(index)
        .expect("pixel index fits u64")
        .wrapping_mul(0x9e37_79b9)
        ^ id.wrapping_mul(0x0001_0101);
    let [b, g, r, ..] = value.to_le_bytes();
    [b, g, r, 255]
}

fn paint_frame(pixels: &mut [u8], width: usize, pattern: Pattern, id: u64) {
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *pixel = match pattern {
            Pattern::Full | Pattern::Resize | Pattern::Tall | Pattern::Wide => hash(index, id),
            Pattern::Sparse => {
                let x = if id & 1 == 0 { 80 } else { 100 };
                let value = if (100..120).contains(&(index / width))
                    && (x..x + 20).contains(&(index % width))
                {
                    173
                } else {
                    31
                };
                [value, value, value, 255]
            }
        };
    }
}

/// Paint a buffer whose only declared damage is `rect`.
///
/// Pixels outside the rectangle always carry frame 0, in every backing buffer,
/// so the declared damage really does describe where the buffer changed.
fn paint_rectangle(
    pixels: &mut [u8],
    width: usize,
    id: u64,
    (x, y, w, h): (usize, usize, usize, usize),
) {
    for (row, line) in pixels.chunks_exact_mut(width * 4).enumerate() {
        for (column, pixel) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let id = if (y..y + h).contains(&row) && (x..x + w).contains(&column) {
                id
            } else {
                0
            };
            *pixel = hash(row * width + column, id);
        }
    }
}
