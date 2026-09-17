//! Blend premultiplied client pixels into an RGB frame and diff its tiles.

use crate::kitty::ImageId;

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

    pub const fn rgb(self, pixel: [u8; 4]) -> [u8; 3] {
        match self {
            Self::Argb8888 | Self::Xrgb8888 => [pixel[2], pixel[1], pixel[0]],
            Self::Abgr8888 | Self::Xbgr8888 => [pixel[0], pixel[1], pixel[2]],
        }
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
        let scale_x = f64::from(src.width) / f64::from(dst.width);
        let scale_y = f64::from(src.height) / f64::from(dst.height);
        let offset_x = f64::mul_add(f64::from(clipped.x - dst.x), scale_x, f64::from(src.x));
        let offset_y = f64::mul_add(f64::from(clipped.y - dst.y), scale_y, f64::from(src.y));
        for row in 0..clipped.height {
            let image_y = f64::mul_add(f64::from(row), scale_y, offset_y) as i32;
            if image_y < 0 || image_y as u32 >= image.height {
                continue;
            }
            let source_row = image_y as usize * image.stride;
            let frame_row = (clipped.y + row as i32) as usize * self.width as usize * BYTES
                + clipped.x as usize * BYTES;
            self.blend_row(
                image,
                source_row,
                frame_row,
                clipped.width,
                scale_x,
                offset_x,
            );
        }
    }

    fn blend_row(
        &mut self,
        image: &Image<'_>,
        source_row: usize,
        frame_row: usize,
        count: u32,
        scale_x: f64,
        offset_x: f64,
    ) {
        let sample = |index: u32| -> [u8; BYTES4] {
            let image_x = f64::mul_add(f64::from(index), scale_x, offset_x) as i64;
            if image_x < 0 || image_x as u32 >= image.width {
                return [0, 0, 0, 0];
            }
            let offset = source_row + image_x as usize * BYTES4;
            let Some(pixel) = image
                .pixels
                .get(offset..)
                .and_then(<[u8]>::first_chunk::<4>)
            else {
                return [0, 0, 0, 0];
            };
            let [red, green, blue] = image.format.rgb(*pixel);
            let alpha = if image.format.opaque() { 255 } else { pixel[3] };
            [red, green, blue, alpha]
        };

        // Copy opaque rows at native scale without blending.
        if image.format.opaque() && (scale_x - 1.0).abs() < f64::EPSILON && offset_x >= 0.0 {
            let source_start = source_row + offset_x as usize * BYTES4;
            let source_length = count as usize * BYTES4;
            let frame_length = count as usize * BYTES;
            if let (Some(source), Some(destination)) = (
                image.pixels.get(source_start..source_start + source_length),
                self.pixels.get_mut(frame_row..frame_row + frame_length),
            ) {
                let source = source.as_chunks::<BYTES4>().0;
                let destination = destination.as_chunks_mut::<BYTES>().0;
                match image.format {
                    SourceFormat::Argb8888 | SourceFormat::Xrgb8888 => {
                        for (source, destination) in source.iter().zip(destination) {
                            *destination = [source[2], source[1], source[0]];
                        }
                    }
                    SourceFormat::Abgr8888 | SourceFormat::Xbgr8888 => {
                        for (source, destination) in source.iter().zip(destination) {
                            destination.copy_from_slice(&source[..BYTES]);
                        }
                    }
                }
                return;
            }
        }

        for index in 0..count {
            let source = sample(index);
            if source[3] == 0 {
                continue;
            }
            let offset = frame_row + index as usize * BYTES;
            let Some(destination) = self.pixels.get_mut(offset..offset + BYTES) else {
                return;
            };
            blend(destination, source);
        }
    }
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

/// One tile of a frame, as the terminal is told about it.
///
/// A frame's tiles are cut out by the compositor and put on the terminal by the
/// presenter, so this is what they pass: where the tile is, and the image the
/// terminal keeps it under. A tile keeps its image across frames, so the
/// terminal replaces the image it already has instead of drawing another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub image: ImageId,
    pub rect: Rect,
}

/// Tracks changed tiles between frames.
#[derive(Debug)]
pub struct Tiles {
    size: (u32, u32),
    grid: (u32, u32),
    previous: Vec<u8>,
    /// Whether the next [`Tiles::diff`] should report every tile as changed.
    stale: bool,
}

impl Tiles {
    pub fn new(frame: &Frame, size: (u32, u32)) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        let grid = (
            frame.width.div_ceil(size.0).max(1),
            frame.height.div_ceil(size.1).max(1),
        );
        Self {
            size,
            grid,
            previous: Vec::new(),
            stale: true,
        }
    }

    /// Fill `changed` with the tiles that differ from the last diff, in grid
    /// order.
    pub fn diff(&mut self, frame: &Frame, changed: &mut Vec<Tile>) {
        if self.previous.len() != frame.pixels.len() {
            self.stale = true;
            self.previous.resize(frame.pixels.len(), 0);
        }
        changed.clear();
        changed.reserve(self.tile_count());
        for index in 0..self.tile_count() {
            let rect = self.tile(frame, index);
            if self.stale || self.tile_differs(frame, rect) {
                changed.push(Tile {
                    image: ImageId::new(index as u32 + 1),
                    rect,
                });
                self.update_previous(frame, rect);
            }
        }
        self.stale = false;
    }

    const fn tile_count(&self) -> usize {
        self.grid.0 as usize * self.grid.1 as usize
    }

    fn tile(&self, frame: &Frame, index: usize) -> Rect {
        let grid_x = index as u32 % self.grid.0;
        let grid_y = index as u32 / self.grid.0;
        let x = grid_x * self.size.0;
        let y = grid_y * self.size.1;
        Rect::new(
            x as i32,
            y as i32,
            self.size.0.min(frame.width - x),
            self.size.1.min(frame.height - y),
        )
    }

    fn tile_differs(&self, frame: &Frame, tile: Rect) -> bool {
        for row in Self::rows(frame, tile) {
            if frame.pixels()[row.clone()] != self.previous[row] {
                return true;
            }
        }
        false
    }

    fn update_previous(&mut self, frame: &Frame, tile: Rect) {
        for row in Self::rows(frame, tile) {
            self.previous[row.clone()].copy_from_slice(&frame.pixels()[row]);
        }
    }

    fn rows(frame: &Frame, tile: Rect) -> impl Iterator<Item = std::ops::Range<usize>> {
        let stride = frame.width as usize * BYTES;
        let first = tile.y as usize * stride + tile.x as usize * BYTES;
        let length = tile.width as usize * BYTES;
        (0..tile.height as usize).map(move |row| {
            let start = first + row * stride;
            start..start + length
        })
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

    #[test]
    fn tiles_only_report_changed_regions() {
        let mut frame = Frame::new(64, 64);
        frame.clear([0, 0, 0]);
        let mut tiles = Tiles::new(&frame, (32, 32));
        let mut changed = Vec::new();
        tiles.diff(&frame, &mut changed);
        assert_eq!(changed.len(), 4, "the first diff is a full repaint");
        tiles.diff(&frame, &mut changed);
        assert!(changed.is_empty(), "an unchanged frame is not sent");
        let red = argb(&[[255, 0, 0, 255]; 4], 2, 2);
        frame.draw(
            &image(&red, 2, 2),
            Rect::new(0, 0, 2, 2),
            Rect::new(40, 40, 2, 2),
        );
        tiles.diff(&frame, &mut changed);
        assert_eq!(
            changed,
            vec![Tile {
                image: ImageId::new(4),
                rect: Rect::new(32, 32, 32, 32),
            }],
            "only the tile holding the change, under the image it keeps"
        );
        tiles.diff(&frame, &mut changed);
        assert_eq!(changed, [] as [Tile; 0]);
    }

    #[test]
    fn a_resized_frame_reports_every_tile() {
        let mut frame = Frame::new(64, 64);
        frame.clear([0, 0, 0]);
        let mut tiles = Tiles::new(&frame, (32, 32));
        let mut changed = Vec::new();
        tiles.diff(&frame, &mut changed);
        frame.resize(32, 32);
        tiles = Tiles::new(&frame, (32, 32));
        tiles.diff(&frame, &mut changed);
        assert_eq!(changed.len(), 1);
    }
}
