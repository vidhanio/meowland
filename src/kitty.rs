//! Encoding of RGB frames for the kitty graphics protocol.
//!
//! `Presenter` deliberately owns the previous frame.  Callers can therefore
//! hand it a frame and immediately reuse their frame storage after this
//! method returns.  The output is one synchronized terminal update.

use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};

const MAX_PATCHES: usize = 32;
const CHUNK: usize = 4096;
const SCREEN_ID: u32 = 1;
const FIRST_PATCH_ID: u32 = 2;

/// Stateful kitty image presenter.
#[derive(Debug, Default)]
pub struct Presenter {
    cell_size: Option<(u16, u16)>,
    previous: Option<Frame>,
    base: Option<Frame>,
    patch_count: usize,
}

#[derive(Debug, Clone)]
struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Presenter {
    /// Construct a presenter.  Patches are enabled only when the terminal's
    /// reported cell dimensions are supplied.
    #[must_use]
    pub const fn new(cell_size: Option<(u16, u16)>) -> Self {
        Self {
            cell_size,
            previous: None,
            base: None,
            patch_count: 0,
        }
    }

    /// Encode a complete frame or a bounded set of cell-aligned patches.
    ///
    /// The RGB buffer must contain exactly `width * height * 3` bytes.  An
    /// invalid buffer is ignored and returns an empty update.
    #[must_use]
    pub fn present(&mut self, width: u32, height: u32, rgb: Vec<u8>) -> Vec<u8> {
        let pixels = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|n| n.checked_mul(3));
        if pixels != Some(rgb.len()) || width == 0 || height == 0 {
            return Vec::new();
        }
        if self
            .previous
            .as_ref()
            .is_some_and(|old| old.width == width && old.height == height && old.pixels == rgb)
        {
            return Vec::new();
        }

        let frame = Frame {
            width,
            height,
            pixels: rgb,
        };
        let mut out = Vec::with_capacity(frame.pixels.len().min(128 * 1024));
        out.extend_from_slice(b"\x1b[?2026h");

        let patches = self
            .base
            .as_ref()
            .filter(|old| old.width == width && old.height == height)
            .and_then(|old| self.changed_rects(old, &frame));
        let use_patches = patches.as_ref().is_some_and(|rects| {
            !rects.is_empty()
                && rects.len() <= MAX_PATCHES
                && self.patch_count + rects.len() <= 64
                && rects
                    .iter()
                    .map(|rect| u64::from(rect.width) * u64::from(rect.height))
                    .sum::<u64>()
                    < u64::from(width) * u64::from(height)
        });

        if use_patches {
            let rects = patches.expect("checked above");
            for index in 0..self.patch_count {
                delete_image(
                    &mut out,
                    FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX),
                );
            }
            for (index, rect) in rects.iter().enumerate() {
                let id = FIRST_PATCH_ID + u32::try_from(index).unwrap_or(u32::MAX);
                let payload = extract(&frame, *rect);
                move_cursor(&mut out, rect.x, rect.y, self.cell_size);
                image(&mut out, id, rect.width, rect.height, &payload, true);
            }
            self.patch_count = rects.len();
        } else {
            delete_all(&mut out);
            out.extend_from_slice(b"\x1b[H");
            image(&mut out, SCREEN_ID, width, height, &frame.pixels, false);
            self.patch_count = 0;
        }

        out.extend_from_slice(b"\x1b[?2026l");
        if self.patch_count == 0 {
            self.base = Some(frame.clone());
        }
        self.previous = Some(frame);
        out
    }

    fn changed_rects(&self, old: &Frame, new: &Frame) -> Option<Vec<Rect>> {
        let (cell_w, cell_h) = self.cell_size?;
        let (cell_w, cell_h) = (u32::from(cell_w), u32::from(cell_h));
        if cell_w == 0 || cell_h == 0 {
            return None;
        }
        let cols = new.width.div_ceil(cell_w);
        let rows = new.height.div_ceil(cell_h);
        let mut rects: Vec<Rect> = Vec::new();
        for row in 0..rows {
            let y = row * cell_h;
            let h = cell_h.min(new.height - y);
            let mut col = 0;
            while col < cols {
                let x = col * cell_w;
                let w = cell_w.min(new.width - x);
                if !different(old, new, x, y, w, h) {
                    col += 1;
                    continue;
                }
                let start = col;
                col += 1;
                while col < cols {
                    let next_x = col * cell_w;
                    let next_w = cell_w.min(new.width - next_x);
                    if !different(old, new, next_x, y, next_w, h) {
                        break;
                    }
                    col += 1;
                }
                let rect = Rect {
                    x: start * cell_w,
                    y,
                    width: ((col - start) * cell_w).min(new.width - start * cell_w),
                    height: h,
                };
                if !rect.width.is_multiple_of(cell_w) || !rect.height.is_multiple_of(cell_h) {
                    return None;
                }
                // Merge vertically adjacent equal-width runs.  This keeps
                // flat UI changes compact without expensive rectangle packing.
                if let Some(last) = rects.last_mut()
                    && last.x == rect.x
                    && last.width == rect.width
                    && last.y + last.height == rect.y
                {
                    last.height += rect.height;
                    continue;
                }
                rects.push(rect);
                if rects.len() > MAX_PATCHES {
                    return Some(rects);
                }
            }
        }
        Some(rects)
    }
}

fn different(old: &Frame, new: &Frame, x: u32, y: u32, width: u32, height: u32) -> bool {
    let old_stride = old.width as usize * 3;
    let new_stride = new.width as usize * 3;
    for row in y as usize..(y + height) as usize {
        let start = x as usize * 3;
        let end = (x + width) as usize * 3;
        if old.pixels[row * old_stride + start..row * old_stride + end]
            != new.pixels[row * new_stride + start..row * new_stride + end]
        {
            return true;
        }
    }
    false
}

fn extract(frame: &Frame, rect: Rect) -> Vec<u8> {
    let stride = frame.width as usize * 3;
    let row_len = rect.width as usize * 3;
    let mut out = Vec::with_capacity(row_len * rect.height as usize);
    for row in rect.y as usize..(rect.y + rect.height) as usize {
        let start = rect.x as usize * 3;
        out.extend_from_slice(&frame.pixels[row * stride + start..row * stride + start + row_len]);
    }
    out
}

fn move_cursor(out: &mut Vec<u8>, x: u32, y: u32, cell_size: Option<(u16, u16)>) {
    let (cell_w, cell_h) = cell_size.map_or((1, 1), |(w, h)| (u32::from(w), u32::from(h)));
    let col = x / cell_w + 1;
    let row = y / cell_h + 1;
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").as_bytes());
}

fn image(out: &mut Vec<u8>, id: u32, width: u32, height: u32, pixels: &[u8], patch: bool) {
    let compressed = compress(pixels);
    let (payload, zlib) = compressed.as_deref().map_or((pixels, false), |candidate| {
        if candidate.len() * 4 <= pixels.len() * 3 {
            (candidate, true)
        } else {
            (pixels, false)
        }
    });
    let encoded = STANDARD.encode(payload);
    let mut first = true;
    let mut offset = 0;
    while offset < encoded.len() {
        let end = (offset + CHUNK).min(encoded.len());
        let more = end < encoded.len();
        if first {
            let placement = if patch { ",p=1" } else { ",p=0" };
            let compression = if zlib { ",o=z" } else { "" };
            let marker = if more { ",m=1" } else { "" };
            out.extend_from_slice(format!("\x1b_Ga=T,f=24,s={width},v={height},i={id}{placement},z=1,C=1,q=2{compression}{marker};").as_bytes());
            first = false;
        }
        out.extend_from_slice(&encoded.as_bytes()[offset..end]);
        out.extend_from_slice(b"\x1b\\");
        offset = end;
        if more {
            let next_more = offset + CHUNK < encoded.len();
            if next_more {
                out.extend_from_slice(b"\x1b_Gm=1;");
            } else {
                out.extend_from_slice(b"\x1b_Gm=0;");
            }
        }
    }
}

fn compress(data: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, io::Read};

    #[derive(Debug)]
    struct DecodedImage {
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        pixels: Vec<u8>,
    }

    fn split_data(body: &[u8]) -> (&[u8], &[u8]) {
        body.iter()
            .position(|byte| *byte == b';')
            .map_or((body, &[]), |index| (&body[..index], &body[index + 1..]))
    }

    /// Small protocol replay used by the tests. It parses APCs independently
    /// of the encoder and models kitty's persistent image layer semantics.
    #[allow(clippy::too_many_lines)]
    fn replay(stream: &[u8], width: usize, height: usize, cell: (usize, usize)) -> Vec<u8> {
        let mut base = vec![0; width * height * 3];
        let mut screen = base.clone();
        let mut images: HashMap<u32, DecodedImage> = HashMap::new();
        let mut cursor = (0usize, 0usize);
        let mut pos = 0;
        while pos < stream.len() {
            if stream[pos..].starts_with(b"\x1b[")
                && let Some(end) = stream[pos + 2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
            {
                let end = pos + 2 + end;
                if stream[end] != b'H' {
                    pos = end + 1;
                    continue;
                }
                let numbers = std::str::from_utf8(&stream[pos + 2..end]).unwrap();
                if numbers.is_empty() {
                    cursor = (0, 0);
                } else {
                    let mut it = numbers
                        .split(';')
                        .map(|part| part.parse::<usize>().unwrap());
                    cursor = (it.next().unwrap() - 1, it.next().unwrap() - 1);
                }
                pos = end + 1;
                continue;
            }
            if !stream[pos..].starts_with(b"\x1b_G") {
                pos += 1;
                continue;
            }
            let start = pos + 3;
            let end = stream[start..]
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .map(|offset| start + offset)
                .unwrap();
            let body = &stream[start..end];
            let (params, data) = split_data(body);
            let params = String::from_utf8_lossy(params);
            let fields: HashMap<_, _> = params
                .split(',')
                .filter_map(|field| field.split_once('='))
                .collect();
            if fields.get("a") == Some(&"d") {
                if fields.get("d") == Some(&"A") {
                    images.clear();
                } else if let Some(id) = fields.get("i").and_then(|id| id.parse().ok()) {
                    images.remove(&id);
                }
                screen = base.clone();
                for image in images.values() {
                    blit(&mut screen, width, image);
                }
                pos = end + 2;
                continue;
            }
            if fields.get("a") != Some(&"T") {
                pos = end + 2;
                continue;
            }
            let id: u32 = fields["i"].parse().unwrap();
            let image_width: usize = fields["s"].parse().unwrap();
            let image_height: usize = fields["v"].parse().unwrap();
            let patch = fields.get("p") == Some(&"1");
            let compressed = fields.get("o") == Some(&"z");
            let mut encoded = data.to_vec();
            let mut next = end + 2;
            let mut more = fields.get("m") == Some(&"1");
            while more {
                assert!(stream[next..].starts_with(b"\x1b_G"));
                let continuation_start = next + 3;
                let continuation_end = stream[continuation_start..]
                    .windows(2)
                    .position(|window| window == b"\x1b\\")
                    .map(|offset| continuation_start + offset)
                    .unwrap();
                let continuation = &stream[continuation_start..continuation_end];
                let (continuation_params, continuation_data) = split_data(continuation);
                encoded.extend_from_slice(continuation_data);
                more = continuation_params == b"m=1";
                next = continuation_end + 2;
            }
            let mut decoded = STANDARD.decode(encoded).unwrap();
            if compressed {
                let compressed = decoded;
                let mut zlib = flate2::read::ZlibDecoder::new(compressed.as_slice());
                decoded = Vec::new();
                zlib.read_to_end(&mut decoded).unwrap();
            }
            if patch {
                let image = DecodedImage {
                    x: cursor.0 * cell.0,
                    y: cursor.1 * cell.1,
                    width: image_width,
                    height: image_height,
                    pixels: decoded,
                };
                images.insert(id, image);
            } else {
                base = decoded;
                screen = base.clone();
                images.clear();
            }
            for image in images.values() {
                blit(&mut screen, width, image);
            }
            pos = next;
        }
        screen
    }

    fn blit(screen: &mut [u8], screen_width: usize, image: &DecodedImage) {
        for row in 0..image.height {
            let destination = ((image.y + row) * screen_width + image.x) * 3;
            let source = row * image.width * 3;
            screen[destination..destination + image.width * 3]
                .copy_from_slice(&image.pixels[source..source + image.width * 3]);
        }
    }

    #[test]
    fn first_frame_is_whole_and_subsequent_cell_change_is_patch() {
        let mut presenter = Presenter::new(Some((2, 2)));
        let first = presenter.present(4, 2, vec![0; 24]);
        assert!(String::from_utf8_lossy(&first).contains("i=1"));
        let mut next = vec![0; 24];
        next[0..3].copy_from_slice(&[255, 0, 0]);
        let second = presenter.present(4, 2, next);
        let text = String::from_utf8_lossy(&second);
        assert!(text.contains("i=2"));
        assert!(text.contains("p=1"));
        assert!(!text.contains("i=1"));
    }

    #[test]
    fn unchanged_frame_is_empty() {
        let mut presenter = Presenter::new(None);
        let rgb = vec![1; 12];
        assert!(!presenter.present(2, 2, rgb.clone()).is_empty());
        assert!(presenter.present(2, 2, rgb).is_empty());
    }

    #[test]
    fn large_payload_is_chunked_at_four_thousand_ninety_six() {
        let mut presenter = Presenter::new(None);
        let mut state = 0x9e37_79b9_u32;
        let rgb: Vec<u8> = (0..30_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let output = presenter.present(100, 100, rgb);
        assert!(String::from_utf8_lossy(&output).contains("\x1b\\\x1b_Gm=1;"));
    }

    #[test]
    fn replayed_whole_and_persistent_patches_match_every_frame() {
        let mut presenter = Presenter::new(Some((2, 2)));
        let frame0 = vec![0; 4 * 4 * 3];
        let mut frame1 = frame0.clone();
        frame1[0..12].fill(200);
        let mut frame2 = frame1.clone();
        frame2[(2 * 4 + 2) * 3..(2 * 4 + 2) * 3 + 12].fill(80);
        let first = presenter.present(4, 4, frame0.clone());
        assert_eq!(replay(&first, 4, 4, (2, 2)), frame0);
        let second = presenter.present(4, 4, frame1.clone());
        assert_eq!(
            replay(&[first.clone(), second.clone()].concat(), 4, 4, (2, 2)),
            frame1
        );
        let third = presenter.present(4, 4, frame2.clone());
        assert_eq!(
            replay(&[first, second, third].concat(), 4, 4, (2, 2)),
            frame2
        );
    }

    #[test]
    #[ignore = "manual performance check"]
    fn benchmark_present_1080p_and_small_patch() {
        let mut presenter = Presenter::new(Some((10, 20)));
        let frame = vec![17; 1920 * 1080 * 3];
        let start = std::time::Instant::now();
        let whole = presenter.present(1920, 1080, frame.clone());
        let whole_time = start.elapsed();
        let mut changed = frame;
        changed[0..300].fill(42);
        let start = std::time::Instant::now();
        let patch = presenter.present(1920, 1080, changed);
        eprintln!(
            "kitty benchmark: whole={} bytes in {:?}; patch={} bytes in {:?}",
            whole.len(),
            whole_time,
            patch.len(),
            start.elapsed()
        );
    }
}
