//! Inline kitty transfers with reusable compression storage.

use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};

const CHUNK: usize = 4096;
/// Base64 payload bytes per 4096-character chunk.
const CHUNK_PAYLOAD: usize = CHUNK / 4 * 3;

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Raw,
    Zlib,
}

#[derive(Debug, Default)]
pub(crate) struct Encoder {
    compressed: Vec<u8>,
}

impl Encoder {
    fn prepare(&mut self, pixels: &[u8]) -> Encoding {
        if self.compress(pixels) && self.compressed.len() * 4 <= pixels.len() * 3 {
            Encoding::Zlib
        } else {
            Encoding::Raw
        }
    }

    fn payload<'a>(&'a self, pixels: &'a [u8], encoding: Encoding) -> &'a [u8] {
        match encoding {
            Encoding::Raw => pixels,
            Encoding::Zlib => &self.compressed,
        }
    }

    pub(crate) fn image(
        &mut self,
        out: &mut Vec<u8>,
        id: u32,
        size: (u32, u32),
        pixels: &[u8],
        placement: Placement,
    ) {
        let encoding = self.prepare(pixels);
        self.write(out, id, size, pixels, placement, encoding);
    }

    fn write(
        &self,
        out: &mut Vec<u8>,
        id: u32,
        size: (u32, u32),
        pixels: &[u8],
        placement: Placement,
        encoding: Encoding,
    ) {
        let payload = self.payload(pixels, encoding);
        out.reserve(encoded_capacity(payload.len()));
        let mut encoded_chunk = [0u8; CHUNK];
        let mut chunks = payload.chunks(CHUNK_PAYLOAD).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let bytes = STANDARD
                .encode_slice(chunk, &mut encoded_chunk)
                .expect("a chunk of payload always fits its base64");
            let more = chunks.peek().is_some();
            if first {
                out.extend_from_slice(b"\x1b_G");
                transmit(out, id, size.0, size.1, placement);
                if matches!(encoding, Encoding::Zlib) {
                    out.extend_from_slice(b",o=z");
                }
                if more {
                    out.extend_from_slice(b",m=1");
                }
                out.push(b';');
                first = false;
            } else {
                out.extend_from_slice(placement.continuation());
                out.extend_from_slice(if more { b"m=1;" } else { b"m=0;" });
            }
            out.extend_from_slice(&encoded_chunk[..bytes]);
            out.extend_from_slice(b"\x1b\\");
        }
    }

    /// Sample large frames before spending a full compression pass on noise.
    fn compress(&mut self, pixels: &[u8]) -> bool {
        const SAMPLE_BYTES: usize = 16 * 1024;
        const SAMPLES: usize = 4;
        const SAMPLED_BYTES: usize = SAMPLE_BYTES * SAMPLES;
        if pixels.len() > SAMPLED_BYTES {
            let samples = (0..SAMPLES).map(|index| {
                let start = (pixels.len() - SAMPLE_BYTES) * index / (SAMPLES - 1);
                &pixels[start..start + SAMPLE_BYTES]
            });
            if !Self::zlib(&mut self.compressed, samples)
                || self.compressed.len() * 4 > SAMPLED_BYTES * 3
            {
                return false;
            }
        }
        Self::zlib(&mut self.compressed, [pixels])
    }

    /// Compress the concatenation of `parts` into `out`.
    fn zlib<'a>(out: &mut Vec<u8>, parts: impl IntoIterator<Item = &'a [u8]>) -> bool {
        out.clear();
        let mut encoder = ZlibEncoder::new(out, Compression::fast());
        parts
            .into_iter()
            .all(|part| encoder.write_all(part).is_ok())
            && encoder.finish().is_ok()
    }
}

fn encoded_capacity(payload_len: usize) -> usize {
    let chunks = payload_len.div_ceil(CHUNK_PAYLOAD).max(1);
    payload_len.div_ceil(3) * 4 + chunks * 16 + 96
}

/// Whether a transfer replaces the whole placement or edits the root frame in
/// place.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Placement {
    Whole,
    Edit { x: u32, y: u32 },
}

impl Placement {
    const fn continuation(self) -> &'static [u8] {
        match self {
            Self::Whole => b"\x1b_G",
            Self::Edit { .. } => b"\x1b_Ga=f,",
        }
    }
}

/// Common Kitty transmission keys; callers append transport and chunk keys.
pub(crate) fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, placement: Placement) {
    match placement {
        Placement::Whole => write!(out, "a=T,f=24,s={width},v={height},i={id},p=1,z=1,C=1,q=2"),
        Placement::Edit { x, y } => write!(
            out,
            "a=f,f=24,s={width},v={height},i={id},r=1,x={x},y={y},X=1,q=2"
        ),
    }
    .expect("writing to Vec cannot fail");
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read;

    use flate2::read::ZlibDecoder;

    use super::*;

    pub(crate) fn decode_image(output: &[u8]) -> (String, Vec<u8>, usize) {
        let text = std::str::from_utf8(output).expect("Kitty output is ASCII");
        let mut rest = text;
        let mut header = String::new();
        let mut payload = Vec::new();
        let mut count = 0;
        let mut previous_more = false;
        while let Some(start) = rest.find("\x1b_G") {
            rest = &rest[start + 3..];
            let (command, remaining) = rest.split_once("\x1b\\").expect("terminated Kitty chunk");
            let (keys, encoded) = command.split_once(';').expect("chunk has payload");
            if count == 0 {
                header = keys.to_owned();
            } else {
                assert!(previous_more, "continuation without m=1");
                assert!(keys == "m=1" || keys == "m=0" || keys == "a=f,m=1" || keys == "a=f,m=0");
            }
            assert!(encoded.len() <= 4096);
            previous_more = keys.split(',').any(|key| key == "m=1");
            if previous_more {
                assert_eq!(encoded.len(), 4096);
            }
            payload.extend(STANDARD.decode(encoded).expect("valid base64 chunk"));
            count += 1;
            rest = remaining;
        }
        assert!(count > 0, "missing Kitty image");
        assert!(!previous_more, "unterminated Kitty transfer");
        if header.split(',').any(|key| key == "o=z") {
            let mut decoded = Vec::new();
            ZlibDecoder::new(payload.as_slice())
                .read_to_end(&mut decoded)
                .expect("valid zlib payload");
            payload = decoded;
        }
        (header, payload, count)
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn raw_chunks_round_trip_at_payload_boundaries() {
        for len in [3071, 3072, 3073, 6144, 6145] {
            let pixels = noise(len);
            for placement in [Placement::Whole, Placement::Edit { x: 2, y: 3 }] {
                let mut output = Vec::new();
                Encoder::default().image(&mut output, 7, (17, 13), &pixels, placement);
                let (header, decoded, chunks) = decode_image(&output);
                assert!(!header.contains("o=z"), "length {len}");
                assert_eq!(decoded, pixels, "length {len}");
                assert_eq!(chunks, len.div_ceil(CHUNK_PAYLOAD), "length {len}");
                match placement {
                    Placement::Whole => assert!(header.contains("a=T")),
                    Placement::Edit { .. } => {
                        assert!(header.contains("a=f") && header.contains("x=2,y=3"));
                    }
                }
            }
        }
    }

    #[test]
    fn compressed_multichunk_payload_round_trips() {
        let mut pixels = noise(9000);
        pixels.extend(vec![0; 9000]);
        for placement in [Placement::Whole, Placement::Edit { x: 2, y: 3 }] {
            let mut output = Vec::new();
            Encoder::default().image(&mut output, 7, (100, 60), &pixels, placement);
            let (header, decoded, chunks) = decode_image(&output);
            assert!(header.contains("o=z"));
            assert!(chunks > 1);
            assert_eq!(decoded, pixels);
        }
    }
}
