use super::config::Size;

pub(crate) const APP_ID: &str = "meowland-e2e";
pub(crate) const LOAD_BIT: u64 = 1 << 63;
const MAGIC: [u8; 4] = *b"MEOW";
const CHECK: u64 = 0x91a7_485e;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Pattern {
    Noise,
    Damage,
}

pub(crate) fn marker(id: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4..12].copy_from_slice(&id.to_le_bytes());
    bytes[12..].copy_from_slice(&(id ^ (id >> 32) ^ CHECK).to_le_bytes()[..4]);
    bytes
}

pub(crate) fn identify(pixels: &[u8]) -> Option<u64> {
    if pixels.len() != 48 {
        return None;
    }
    let mut bytes = [0; 16];
    for (value, rgb) in bytes.iter_mut().zip(pixels.as_chunks::<3>().0) {
        if rgb[0] != rgb[1] || rgb[0] != rgb[2] {
            return None;
        }
        *value = rgb[0];
    }
    let id = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    (bytes == marker(id)).then_some(id)
}

pub(crate) struct Fixture {
    base: Vec<u8>,
    size: Size,
    pattern: Pattern,
}

impl Fixture {
    pub(crate) fn new(size: Size, pattern: Pattern) -> Self {
        let mut seed = 7_u32;
        let mut base = vec![0; size.pixels() * 4];
        for pixel in base.as_chunks_mut::<4>().0 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let bytes = if matches!(pattern, Pattern::Noise) {
                seed.to_le_bytes()
            } else {
                [31; 4]
            };
            *pixel = [bytes[0], bytes[1], bytes[2], 255];
        }
        Self {
            base,
            size,
            pattern,
        }
    }

    pub(crate) fn damage(&self) -> Vec<(usize, usize, usize, usize)> {
        match self.pattern {
            Pattern::Noise => vec![(0, 0, self.size.width, self.size.height)],
            Pattern::Damage => vec![(0, 0, 16, 1), (80, 100, 40, 20)],
        }
    }

    /// Varies every noise pixel by commit sequence, so coalescing cannot turn
    /// full frames into marker-only updates.
    pub(crate) fn paint(&self, id: u64, content: u64, pixels: &mut [u8]) {
        if matches!(self.pattern, Pattern::Noise) {
            let [b, g, r, ..] = content.to_le_bytes();
            let mask = u32::from_le_bytes([b, g, r, 0]);
            for (pixel, base) in pixels
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(self.base.as_chunks::<4>().0)
            {
                *pixel = (u32::from_le_bytes(*base) ^ mask).to_le_bytes();
            }
        } else {
            pixels.copy_from_slice(&self.base);
            let x = if content & 1 == 0 { 80 } else { 100 };
            for y in 100..120 {
                let start = (y * self.size.width + x) * 4;
                pixels[start..start + 20 * 4].fill(173);
            }
        }
        for (pixel, value) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(marker(id)) {
            pixel.copy_from_slice(&[value, value, value, 255]);
        }
    }
}
