//! Inline kitty transfers with reusable compression storage.

use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};

use super::transmit;

const CHUNK: usize = 4096;
/// Base64 payload bytes per 4096-character chunk.
const CHUNK_PAYLOAD: usize = CHUNK / 4 * 3;

#[derive(Clone, Copy, Debug)]
pub(super) enum Encoding {
    Raw,
    Zlib,
}

#[derive(Debug, Default)]
pub(super) struct Encoder {
    compressed: Vec<u8>,
    sample: Vec<u8>,
}

impl Encoder {
    pub(super) fn prepare(&mut self, pixels: &[u8]) -> Encoding {
        if self.compress(pixels) && self.compressed.len() * 4 <= pixels.len() * 3 {
            Encoding::Zlib
        } else {
            Encoding::Raw
        }
    }

    pub(super) fn capacity(&self, pixels: &[u8], encoding: Encoding) -> usize {
        encoded_capacity(self.payload(pixels, encoding).len())
    }

    fn payload<'a>(&'a self, pixels: &'a [u8], encoding: Encoding) -> &'a [u8] {
        match encoding {
            Encoding::Raw => pixels,
            Encoding::Zlib => &self.compressed,
        }
    }

    pub(super) fn image(
        &mut self,
        out: &mut Vec<u8>,
        id: u32,
        size: (u32, u32),
        pixels: &[u8],
        patch: bool,
    ) {
        let encoding = self.prepare(pixels);
        self.write(out, id, size, pixels, patch, encoding);
    }

    /// Write the most recently prepared image without repeating compression.
    pub(super) fn write(
        &self,
        out: &mut Vec<u8>,
        id: u32,
        size: (u32, u32),
        pixels: &[u8],
        patch: bool,
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
                transmit(out, id, size.0, size.1, u32::from(patch));
                if matches!(encoding, Encoding::Zlib) {
                    out.extend_from_slice(b",o=z");
                }
                if more {
                    out.extend_from_slice(b",m=1");
                }
                out.push(b';');
                first = false;
            } else if more {
                out.extend_from_slice(b"\x1b_Gm=1;");
            } else {
                out.extend_from_slice(b"\x1b_Gm=0;");
            }
            out.extend_from_slice(&encoded_chunk[..bytes]);
            out.extend_from_slice(b"\x1b\\");
        }
    }

    /// Sample large frames before spending a full compression pass on noise.
    fn compress(&mut self, pixels: &[u8]) -> bool {
        const SAMPLE_BYTES: usize = 16 * 1024;
        const SAMPLES: usize = 4;
        if pixels.len() > SAMPLE_BYTES * SAMPLES {
            self.sample.clear();
            self.sample.reserve(SAMPLE_BYTES * SAMPLES);
            for index in 0..SAMPLES {
                let start = (pixels.len() - SAMPLE_BYTES) * index / (SAMPLES - 1);
                self.sample
                    .extend_from_slice(&pixels[start..start + SAMPLE_BYTES]);
            }
            if !Self::zlib(&mut self.compressed, &self.sample)
                || self.compressed.len() * 4 > self.sample.len() * 3
            {
                return false;
            }
        }
        Self::zlib(&mut self.compressed, pixels)
    }

    fn zlib(out: &mut Vec<u8>, pixels: &[u8]) -> bool {
        out.clear();
        let mut encoder = ZlibEncoder::new(out, Compression::fast());
        encoder.write_all(pixels).is_ok() && encoder.finish().is_ok()
    }
}

fn encoded_capacity(payload_len: usize) -> usize {
    let chunks = payload_len.div_ceil(CHUNK_PAYLOAD).max(1);
    payload_len.div_ceil(3) * 4 + chunks * 16 + 96
}
