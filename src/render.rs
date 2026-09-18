//! Blend premultiplied client pixels into the RGB frame of a pane.

pub const BYTES4: usize = 4;

pub const BYTES: usize = 3;

/// The origin is signed, because a sub-surface offset can be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn intersect(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width as i32).min(other.x + other.width as i32);
        let bottom = (self.y + self.height as i32).min(other.y + other.height as i32);
        (right > x && bottom > y).then(|| Self::new(x, y, (right - x) as u32, (bottom - y) as u32))
    }

    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Pixel layouts, named by channel order in a little-endian word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// `ARGB8888`: bytes are blue, green, red, alpha.
    Argb8888,
    /// `XRGB8888`: bytes are blue, green, red, and the fourth is ignored.
    Xrgb8888,
    /// `ABGR8888`: bytes are red, green, blue, alpha.
    Abgr8888,
    /// `XBGR8888`: bytes are red, green, blue, and the fourth is ignored.
    Xbgr8888,
}

impl SourceFormat {
    pub const fn opaque(self) -> bool {
        matches!(self, Self::Xrgb8888 | Self::Xbgr8888)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub pixels: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub format: SourceFormat,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * BYTES],
        }
    }

    /// Resize the storage. The caller repaints the frame before reading it.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels
            .resize(width as usize * height as usize * BYTES, 0);
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub const fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn clear(&mut self, color: [u8; 3]) {
        for pixel in self.pixels.as_chunks_mut::<BYTES>().0 {
            pixel.copy_from_slice(&color);
        }
    }

    /// Draw `src` into `dst` with nearest-neighbor scaling and frame clipping.
    pub fn draw(&mut self, image: &Image<'_>, src: Rect, dst: Rect) {
        if src.is_empty() || dst.is_empty() {
            return;
        }
        let Some(clipped) = dst.intersect(self.bounds()) else {
            return;
        };
        // An opaque image shown at its own size is a copy of its rows: no pixel
        // has to be chosen.
        if image.format.opaque()
            && src.width == dst.width
            && src.height == dst.height
            && self.copy(image, clipped, src, dst)
        {
            return;
        }
        self.sample(image, clipped, src, dst);
    }

    /// Copy the rows of an image that is shown at its own size, and say whether
    /// it could be: an image whose pixels do not reach the rows and columns the
    /// destination asks for is sampled instead.
    fn copy(&mut self, image: &Image<'_>, clipped: Rect, src: Rect, dst: Rect) -> bool {
        let base_x = src.x + clipped.x - dst.x;
        let base_y = src.y + clipped.y - dst.y;
        if base_x < 0 || base_y < 0 {
            return false;
        }
        let last_row = base_y as usize + clipped.height as usize - 1;
        let needed = last_row * image.stride + (base_x as usize + clipped.width as usize) * BYTES4;
        if last_row as u32 >= image.height || image.pixels.len() < needed {
            return false;
        }
        let swap = matches!(
            image.format,
            SourceFormat::Abgr8888 | SourceFormat::Xbgr8888
        );
        for row in 0..clipped.height as usize {
            let source_start = (base_y as usize + row) * image.stride + base_x as usize * BYTES4;
            let source =
                &image.pixels[source_start..source_start + clipped.width as usize * BYTES4];
            let frame_start = (clipped.y as usize + row) * self.width as usize * BYTES
                + clipped.x as usize * BYTES;
            let destination =
                &mut self.pixels[frame_start..frame_start + clipped.width as usize * BYTES];
            for (source, destination) in source
                .as_chunks::<BYTES4>()
                .0
                .iter()
                .zip(destination.as_chunks_mut::<BYTES>().0)
            {
                *destination = if swap {
                    [source[0], source[1], source[2]]
                } else {
                    [source[2], source[1], source[0]]
                };
            }
        }
        true
    }

    /// Draw an image that is scaled into the frame, a row at a time.
    ///
    /// A destination pixel takes the image pixel at `numerator / denominator`,
    /// where the numerator steps by the image's size every pixel: the mapping
    /// is exact, and stepping it costs an addition a pixel. Nothing else about
    /// a pixel is decided in the loop — the format, and whether it has an alpha
    /// to blend with, are the image's, and are settled once.
    fn sample(&mut self, image: &Image<'_>, clipped: Rect, src: Rect, dst: Rect) {
        let (across, down) = (i64::from(src.width), i64::from(src.height));
        let (columns, rows) = (i64::from(dst.width), i64::from(dst.height));
        let numerator_x = i64::from(clipped.x - dst.x) * across + i64::from(src.x) * columns;
        let numerator_y = i64::from(clipped.y - dst.y) * down + i64::from(src.y) * rows;
        // The destination columns whose image pixels are inside the image:
        // outside them there is nothing to draw, and the frame keeps what it
        // had.
        let first = ceil_div(-numerator_x, across).max(0);
        let last = ceil_div(i64::from(image.width) * columns - numerator_x, across)
            .clamp(first, i64::from(clipped.width));
        if first == last {
            return;
        }

        let opaque = image.format.opaque();
        let swap = matches!(
            image.format,
            SourceFormat::Abgr8888 | SourceFormat::Xbgr8888
        );
        let mut row_numerator = numerator_y;
        for row in 0..clipped.height as usize {
            let image_y = row_numerator.div_euclid(rows);
            row_numerator += down;
            if image_y < 0 || image_y >= i64::from(image.height) {
                continue;
            }
            let source_row = image_y as usize * image.stride;
            let Some(source) = image
                .pixels
                .get(source_row..source_row + image.width as usize * BYTES4)
            else {
                continue;
            };
            let frame_start = (clipped.y as usize + row) * self.width as usize * BYTES
                + (clipped.x as usize + first as usize) * BYTES;
            let length = (last - first) as usize * BYTES;
            let Some(destination) = self.pixels.get_mut(frame_start..frame_start + length) else {
                continue;
            };

            let numerator = numerator_x + first * across;
            let mut image_x = numerator.div_euclid(columns);
            let mut error = numerator.rem_euclid(columns);
            for pixel in destination.as_chunks_mut::<BYTES>().0 {
                // `image_x` is inside the image: `first` and `last` are where
                // it enters and leaves.
                let at = image_x as usize * BYTES4;
                let source = &source[at..at + BYTES4];
                let (red, green, blue) = if swap {
                    (source[0], source[1], source[2])
                } else {
                    (source[2], source[1], source[0])
                };
                let alpha = if opaque { 255 } else { source[3] };
                if alpha != 0 {
                    blend(pixel, [red, green, blue, alpha]);
                }
                error += across;
                while error >= columns {
                    error -= columns;
                    image_x += 1;
                }
            }
        }
    }
}

/// The smallest whole number at or above `a / b`, for a positive `b`.
const fn ceil_div(a: i64, b: i64) -> i64 {
    -((-a).div_euclid(b))
}

/// Blend premultiplied source RGB over opaque destination RGB.
fn blend(destination: &mut [u8], source: [u8; BYTES4]) {
    let alpha = u32::from(source[3]);
    if alpha == 255 {
        destination.copy_from_slice(&source[..BYTES]);
        return;
    }
    let inverse = 255 - alpha;
    for channel in 0..BYTES {
        destination[channel] =
            (u32::from(source[channel]) + u32::from(destination[channel]) * inverse / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour at a position, in the frame's byte order.
    fn sample(frame: &Frame, x: i32, y: i32) -> Option<[u8; BYTES]> {
        if !frame.bounds().contains(x, y) {
            return None;
        }
        let offset = (y as usize * frame.width as usize + x as usize) * BYTES;
        frame
            .pixels()
            .get(offset..offset + BYTES)
            .and_then(|pixel| <[u8; BYTES]>::try_from(pixel).ok())
    }

    fn rgba(image: &Image<'_>, x: u32, y: u32) -> [u8; 4] {
        let offset = y as usize * image.stride + x as usize * 4;
        let pixel = &image.pixels[offset..offset + 4];
        [pixel[2], pixel[1], pixel[0], pixel[3]]
    }

    /// A buffer that holds `pixels` as ARGB8888.
    ///
    /// The bytes are blue, green, red, alpha, and the colors are premultiplied,
    /// as the protocol says clients must provide them.
    fn argb(pixels: &[[u8; 4]], width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for pixel in pixels {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        assert_eq!(bytes.len(), width as usize * height as usize * 4);
        bytes
    }

    fn image(bytes: &[u8], width: u32, height: u32) -> Image<'_> {
        Image {
            pixels: bytes,
            stride: width as usize * 4,
            width,
            height,
            format: SourceFormat::Argb8888,
        }
    }

    #[test]
    fn opaque_pixels_replace_what_is_underneath() {
        let mut frame = Frame::new(4, 4);
        frame.clear([0, 0, 0]);
        let bytes = argb(&[[10, 20, 30, 255]; 4], 2, 2);
        frame.draw(
            &image(&bytes, 2, 2),
            Rect::new(0, 0, 2, 2),
            Rect::new(1, 1, 2, 2),
        );
        assert_eq!(sample(&frame, 1, 1), Some([10, 20, 30]));
        assert_eq!(sample(&frame, 0, 0), Some([0, 0, 0]));
    }

    #[test]
    fn translucent_pixels_blend_towards_the_backdrop() {
        let mut frame = Frame::new(2, 2);
        frame.clear([0, 100, 200]);
        // Half-transparent white, premultiplied as the protocol requires: 128
        // in each color channel, with alpha 128. The backdrop comes through at
        // half strength.
        let bytes = argb(&[[128, 128, 128, 128]; 1], 1, 1);
        frame.draw(
            &image(&bytes, 1, 1),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 1, 1),
        );
        let pixel = sample(&frame, 0, 0).unwrap();
        assert_eq!(pixel[0], 128);
        assert!((177..=178).contains(&pixel[1]), "got {pixel:?}");
        assert!((227..=228).contains(&pixel[2]), "got {pixel:?}");
    }

    #[test]
    fn xrgb_buffers_are_drawn_fully_opaque() {
        let mut frame = Frame::new(2, 2);
        frame.clear([1, 2, 3]);
        let mut bytes = argb(&[[9, 8, 7, 0]], 1, 1);
        bytes[3] = 0; // alpha byte is ignored in XRGB8888
        let mut buffer = image(&bytes, 1, 1);
        buffer.format = SourceFormat::Xrgb8888;
        frame.draw(&buffer, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 1, 1));
        assert_eq!(sample(&frame, 0, 0), Some([9, 8, 7]));
    }

    #[test]
    fn a_readback_is_red_green_blue_whichever_way_the_client_had_it() {
        // Both layouts describe the same color: a client's buffer holds it with
        // blue first, and a buffer brought back through the renderer holds it
        // with red first. The frame ends up the same either way.
        let (blue_first, red_first) = ([30, 20, 10, 255], [10, 20, 30, 255]);

        for (format, bytes) in [
            (SourceFormat::Argb8888, blue_first),
            (SourceFormat::Abgr8888, red_first),
        ] {
            let mut frame = Frame::new(1, 1);
            frame.clear([0, 0, 0]);
            let mut buffer = image(&bytes, 1, 1);
            buffer.format = format;
            frame.draw(&buffer, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 1, 1));
            assert_eq!(sample(&frame, 0, 0), Some([10, 20, 30]), "{format:?}");
        }

        for (format, bytes) in [
            (SourceFormat::Xrgb8888, [30, 20, 10, 0]),
            (SourceFormat::Xbgr8888, [10, 20, 30, 0]),
        ] {
            let mut frame = Frame::new(1, 1);
            frame.clear([0, 0, 0]);
            let mut buffer = image(&bytes, 1, 1);
            buffer.format = format;
            frame.draw(&buffer, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 1, 1));
            assert_eq!(sample(&frame, 0, 0), Some([10, 20, 30]), "{format:?}");
        }
    }

    #[test]
    fn drawing_is_clipped_to_the_frame() {
        let mut frame = Frame::new(4, 4);
        frame.clear([0, 0, 0]);
        let bytes = argb(&[[255, 255, 255, 255]; 4], 2, 2);
        // Straddle the left and top edges. Only the in-frame quarter is drawn.
        frame.draw(
            &image(&bytes, 2, 2),
            Rect::new(0, 0, 2, 2),
            Rect::new(-1, -1, 2, 2),
        );
        assert_eq!(sample(&frame, 0, 0), Some([255, 255, 255]));
        assert_eq!(sample(&frame, 1, 0), Some([0, 0, 0]));
        assert_eq!(sample(&frame, 0, 1), Some([0, 0, 0]));
    }

    #[test]
    fn drawing_is_clipped_to_the_image() {
        let mut frame = Frame::new(2, 1);
        frame.clear([1, 2, 3]);
        let bytes = argb(&[[10, 20, 30, 255], [40, 50, 60, 255]], 2, 1);
        frame.draw(
            &image(&bytes, 2, 1),
            Rect::new(-1, 0, 2, 1),
            Rect::new(0, 0, 2, 1),
        );
        assert_eq!(sample(&frame, 0, 0), Some([1, 2, 3]));
        assert_eq!(sample(&frame, 1, 0), Some([10, 20, 30]));
    }

    #[test]
    fn scaling_uses_the_source_region() {
        let mut frame = Frame::new(4, 4);
        frame.clear([0, 0, 0]);
        // A two by two image with only the bottom-right pixel white, shown at
        // 4x4.
        let bytes = argb(
            &[
                [0, 0, 0, 255],
                [0, 0, 0, 255],
                [0, 0, 0, 255],
                [255, 255, 255, 255],
            ],
            2,
            2,
        );
        let mut frame_image = image(&bytes, 2, 2);
        frame_image.stride = 8;
        frame.draw(&frame_image, Rect::new(0, 0, 2, 2), Rect::new(0, 0, 4, 4));
        assert_eq!(sample(&frame, 0, 0), Some([0, 0, 0]));
        assert_eq!(sample(&frame, 2, 2), Some([255, 255, 255]));
        assert_eq!(sample(&frame, 3, 3), Some([255, 255, 255]));
        assert_eq!(rgba(&frame_image, 1, 1), [255, 255, 255, 255]);
    }

    /// Where a destination pixel reads from and what lands there, the slow way:
    /// the mapping in whole numbers, one pixel at a time.
    fn reference(frame: &mut Frame, image: &Image<'_>, src: Rect, dst: Rect) {
        let Some(clipped) = dst.intersect(frame.bounds()) else {
            return;
        };
        let (across, down) = (i64::from(src.width), i64::from(src.height));
        let (columns, rows) = (i64::from(dst.width), i64::from(dst.height));
        for row in 0..i64::from(clipped.height) {
            let y = ((row + i64::from(clipped.y - dst.y)) * down + i64::from(src.y) * rows)
                .div_euclid(rows);
            if y < 0 || y >= i64::from(image.height) {
                continue;
            }
            for column in 0..i64::from(clipped.width) {
                let x = ((column + i64::from(clipped.x - dst.x)) * across
                    + i64::from(src.x) * columns)
                    .div_euclid(columns);
                if x < 0 || x >= i64::from(image.width) {
                    continue;
                }
                let at = y as usize * image.stride + x as usize * BYTES4;
                let Some(pixel) = image.pixels.get(at..at + BYTES4) else {
                    continue;
                };
                let (red, green, blue) = match image.format {
                    SourceFormat::Argb8888 | SourceFormat::Xrgb8888 => {
                        (pixel[2], pixel[1], pixel[0])
                    }
                    SourceFormat::Abgr8888 | SourceFormat::Xbgr8888 => {
                        (pixel[0], pixel[1], pixel[2])
                    }
                };
                let alpha = if image.format.opaque() { 255 } else { pixel[3] };
                if alpha == 0 {
                    continue;
                }
                let at = ((clipped.y as usize + row as usize) * frame.width as usize
                    + clipped.x as usize
                    + column as usize)
                    * BYTES;
                blend(&mut frame.pixels[at..at + BYTES], [red, green, blue, alpha]);
            }
        }
    }

    #[test]
    fn a_draw_lands_in_the_pixels_the_mapping_says() {
        // Every way a window can be put on a pane: scaled up and down, at its
        // own size, off the edges, and showing part of its buffer.
        let (image_width, image_height) = (7u32, 5u32);
        let mut bytes = Vec::new();
        for y in 0..image_height {
            for x in 0..image_width {
                let seed = (x * 31 + y * 17) as u8;
                let alpha = match (x + y) % 3 {
                    0 => 0,
                    1 => 128,
                    _ => 255,
                };
                bytes.extend_from_slice(&[seed, seed.wrapping_mul(3), seed.wrapping_add(9), alpha]);
            }
        }
        let rects = [
            (Rect::new(0, 0, 7, 5), Rect::new(0, 0, 7, 5)),
            (Rect::new(0, 0, 7, 5), Rect::new(2, 3, 7, 5)),
            (Rect::new(0, 0, 7, 5), Rect::new(0, 0, 20, 15)),
            (Rect::new(0, 0, 7, 5), Rect::new(0, 0, 3, 2)),
            (Rect::new(0, 0, 7, 5), Rect::new(-2, -1, 10, 7)),
            (Rect::new(0, 0, 7, 5), Rect::new(15, 10, 10, 8)),
            (Rect::new(2, 1, 3, 2), Rect::new(5, 4, 9, 7)),
            (Rect::new(1, 0, 1, 5), Rect::new(0, 0, 5, 5)),
            (Rect::new(-1, 0, 4, 3), Rect::new(0, 0, 4, 3)),
            (Rect::new(5, 3, 4, 4), Rect::new(0, 0, 8, 8)),
            // Native across but scaled down, and the other way round: these
            // are sampled, not copied.
            (Rect::new(0, 0, 7, 5), Rect::new(4, 4, 7, 11)),
            (Rect::new(0, 0, 7, 5), Rect::new(4, 4, 3, 5)),
        ];
        for format in [
            SourceFormat::Argb8888,
            SourceFormat::Xrgb8888,
            SourceFormat::Abgr8888,
            SourceFormat::Xbgr8888,
        ] {
            for (src, dst) in rects {
                let buffer = Image {
                    pixels: &bytes,
                    stride: image_width as usize * BYTES4,
                    width: image_width,
                    height: image_height,
                    format,
                };
                let mut fast = Frame::new(23, 17);
                let mut slow = Frame::new(23, 17);
                fast.clear([4, 5, 6]);
                slow.clear([4, 5, 6]);
                fast.draw(&buffer, src, dst);
                reference(&mut slow, &buffer, src, dst);
                assert_eq!(
                    fast.pixels(),
                    slow.pixels(),
                    "{format:?} {src:?} into {dst:?}"
                );
            }
        }
    }
}
