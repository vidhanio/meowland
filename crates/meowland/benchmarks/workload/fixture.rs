#[derive(Clone, Copy)]
pub(crate) enum Pattern {
    Full,
    Sparse,
    Resize,
}

pub(crate) const APP_ID: &str = "meowland-workload";

pub(crate) fn marker(id: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(b"MEOW");
    bytes[4..12].copy_from_slice(&id.to_le_bytes());
    bytes[12..].copy_from_slice(&(id ^ (id >> 32) ^ 0x91a7_485e).to_le_bytes()[..4]);
    bytes
}

pub(crate) fn paint(pixels: &mut [u8], width: usize, pattern: Pattern, id: u64) {
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let bytes = match pattern {
            Pattern::Full | Pattern::Resize => {
                let value = u64::try_from(index)
                    .expect("pixel index fits u64")
                    .wrapping_mul(0x9e37_79b9)
                    ^ id.wrapping_mul(0x0001_0101);
                let [b, g, r, ..] = value.to_le_bytes();
                [b, g, r, 255]
            }
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
        *pixel = bytes;
    }
    for (pixel, value) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(marker(id)) {
        *pixel = [value, value, value, 255];
    }
}
