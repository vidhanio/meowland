//! The compositor's own frame buffer: everything clients send ends up here, one
//! pixel at a time.
//!
//! There is no GPU involved. Client buffers come from shared memory, get alpha
//! blended into this frame buffer, and the result is then diffed tile by tile
//! so only what changed is re-sent to the terminal (see [`crate::kitty`]).
//!
//! Pixels are stored **premultiplied** RGBA, which is also the format Wayland
//! clients deliver (`wl_shm` says the alpha channel is premultiplied into the
//! color channels), so compositing a buffer into the frame is one multiply-add
//! per channel with no conversion. With an opaque backdrop - what
//! [`Frame::clear`] paints - the alpha channel stays 255 everywhere, which is
//! what lets the frame be handed to the terminal untouched.

/// A rectangle in framebuffer pixels. Signed, because sub-surface offsets can
/// be negative and clipping is easier this way.
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

    /// The overlapping part of two rectangles, if any.
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

/// Pixel layout of a client buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// 32-bit with an alpha channel (`ARGB8888`).
    Argb8888,
    /// 32-bit with the alpha byte ignored (`XRGB8888`): every pixel is opaque.
    Xrgb8888,
}

/// A client buffer's pixels, as read out of shared memory.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub pixels: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub format: SourceFormat,
}

/// The screen we are drawing on.
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
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    /// Resize, discarding the contents.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels.resize(width as usize * height as usize * 4, 0);
        self.pixels.fill(0);
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub const fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Paint the whole frame with an opaque color, the backdrop everything else
    /// blends onto.
    pub fn clear(&mut self, color: [u8; 3]) {
        // The backdrop is opaque, which is what keeps the frame's alpha channel
        // at 255 everywhere and therefore keeps premultiplied ==
        // straight for the terminal.
        for pixel in self.pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }

    /// Blend a client buffer into `dst`.
    ///
    /// `src` selects the region of the image to show (in image pixels) and
    /// `dst` the region of the frame to show it in; when they differ in
    /// size the image is scaled with nearest-neighbour sampling, which is
    /// what buffer scale and viewport scaling come down to. Everything is
    /// clipped to the frame.
    pub fn draw(&mut self, image: &Image<'_>, src: Rect, dst: Rect) {
        if src.is_empty() || dst.is_empty() {
            return;
        }
        let Some(clipped) = dst.intersect(self.bounds()) else {
            return;
        };
        // Map the clipped destination back into image space, so clipped pixels
        // cost nothing.
        let scale_x = f64::from(src.width) / f64::from(dst.width);
        let scale_y = f64::from(src.height) / f64::from(dst.height);
        let offset = [
            f64::from(clipped.x - dst.x) * scale_x,
            f64::from(clipped.y - dst.y) * scale_y,
        ];
        for row in 0..clipped.height {
            let image_y = src.y + f64::mul_add(f64::from(row), scale_y, offset[1]) as i32;
            if image_y < 0 || image_y as u32 >= image.height {
                continue;
            }
            let source_row = image_y as usize * image.stride + src.x as usize * 4;
            let frame_row = (clipped.y + row as i32) as usize * self.width as usize * 4
                + clipped.x as usize * 4;
            self.blend_row(
                image,
                source_row,
                frame_row,
                clipped.width,
                scale_x,
                offset[0],
            );
        }
    }

    /// Blend one row of an image into the frame, sampling horizontally.
    fn blend_row(
        &mut self,
        image: &Image<'_>,
        source_row: usize,
        frame_row: usize,
        count: u32,
        scale_x: f64,
        offset_x: f64,
    ) {
        let sample = |index: u32| -> [u8; 4] {
            let image_x = f64::mul_add(f64::from(index), scale_x, offset_x) as i64;
            if image_x < 0 || image_x as u32 >= image.width {
                return [0, 0, 0, 0];
            }
            let offset = source_row + image_x as usize * 4;
            let Some(bytes) = image.pixels.get(offset..offset + 4) else {
                return [0, 0, 0, 0];
            };
            let alpha = match image.format {
                SourceFormat::Xrgb8888 => 255,
                SourceFormat::Argb8888 => bytes[3],
            };
            [bytes[2], bytes[1], bytes[0], alpha]
        };

        // An exactly one-to-one, fully opaque row is a memcpy: this is the
        // common case for a client that renders at the output's scale
        // with an opaque buffer format.
        if image.format == SourceFormat::Xrgb8888
            && (scale_x - 1.0).abs() < f64::EPSILON
            && offset_x == 0.0
            && count as usize * 4 <= self.pixels.len() - frame_row
            && count as usize * 4 <= image.pixels.len() - source_row
            && source_row + count as usize * 4 <= image.pixels.len()
        {
            for index in 0..count as usize {
                let source = &image.pixels[source_row + index * 4..source_row + index * 4 + 3];
                let destination =
                    &mut self.pixels[frame_row + index * 4..frame_row + index * 4 + 4];
                destination[0] = source[2];
                destination[1] = source[1];
                destination[2] = source[0];
                destination[3] = 255;
            }
            return;
        }

        for index in 0..count {
            let source = sample(index);
            if source[3] == 0 {
                continue;
            }
            let offset = frame_row + index as usize * 4;
            let Some(destination) = self.pixels.get_mut(offset..offset + 4) else {
                return;
            };
            blend(destination, source);
        }
    }
}

/// Blend a premultiplied source over the destination: `dst = src + dst * (1 -
/// alpha)`.
///
/// Both sides are premultiplied, so the source is used as-is; the result stays
/// opaque because the destination (the backdrop) is.
fn blend(destination: &mut [u8], source: [u8; 4]) {
    let alpha = u32::from(source[3]);
    if alpha == 255 {
        destination.copy_from_slice(&[source[0], source[1], source[2], 255]);
        return;
    }
    let inverse = 255 - alpha;
    for channel in 0..3 {
        destination[channel] =
            (u32::from(source[channel]) + u32::from(destination[channel]) * inverse / 255) as u8;
    }
    destination[3] = 255;
}

/// Splits the frame into a grid of tiles and reports which of them changed
/// since the last call.
///
/// Terminals want whole images, so the smallest thing we can update is a tile;
/// keeping them around 15 to 30 cells across means a keystroke in a terminal
/// usually costs one small, very compressible image instead of a whole screen.
#[derive(Debug)]
pub struct Tiles {
    /// Tile size in pixels.
    pub size: (u32, u32),
    /// Tile grid dimensions.
    pub grid: (u32, u32),
    previous: Vec<u8>,
    /// Whether the next [`Tiles::diff`] should report every tile as changed.
    stale: bool,
}

impl Tiles {
    pub fn new(frame: &Frame, size: (u32, u32)) -> Self {
        let grid = (
            frame.width.div_ceil(size.0.max(1)).max(1),
            frame.height.div_ceil(size.1.max(1)).max(1),
        );
        Self {
            size,
            grid,
            previous: Vec::new(),
            stale: true,
        }
    }

    /// Rectangles covering the parts of `frame` that differ from the previous
    /// one.
    pub fn diff(&mut self, frame: &Frame) -> Vec<Rect> {
        if self.previous.len() != frame.pixels.len() {
            self.stale = true;
            self.previous = vec![0; frame.pixels.len()];
        }
        let mut changed = Vec::new();
        for grid_y in 0..self.grid.1 {
            for grid_x in 0..self.grid.0 {
                let tile = self.tile(frame, grid_x, grid_y);
                if self.stale || self.tile_differs(frame, tile) {
                    changed.push(tile);
                }
            }
        }
        self.stale = false;
        if !changed.is_empty() {
            self.previous.copy_from_slice(frame.pixels());
        }
        changed
    }

    /// The rectangle covered by one tile, clipped to the frame.
    fn tile(&self, frame: &Frame, grid_x: u32, grid_y: u32) -> Rect {
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
        for row in 0..tile.height {
            let start =
                (tile.y as usize + row as usize) * frame.width as usize * 4 + tile.x as usize * 4;
            let end = start + tile.width as usize * 4;
            if frame.pixels()[start..end] != self.previous[start..end] {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixel at a position, as (r, g, b, a) in premultiplied form.
    fn sample(frame: &Frame, x: i32, y: i32) -> Option<[u8; 4]> {
        if !frame.bounds().contains(x, y) {
            return None;
        }
        let offset = (y as usize * frame.width as usize + x as usize) * 4;
        frame
            .pixels()
            .get(offset..offset + 4)
            .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
    }

    fn rgba(image: &Image<'_>, x: u32, y: u32) -> [u8; 4] {
        let offset = y as usize * image.stride + x as usize * 4;
        let pixel = &image.pixels[offset..offset + 4];
        [pixel[2], pixel[1], pixel[0], pixel[3]]
    }

    /// A buffer holding `pixels` as ARGB8888 (bytes B, G, R, A, colors
    /// premultiplied, as the protocol says clients must provide them).
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
        assert_eq!(sample(&frame, 1, 1), Some([10, 20, 30, 255]));
        assert_eq!(sample(&frame, 0, 0), Some([0, 0, 0, 255]));
    }

    #[test]
    fn translucent_pixels_blend_towards_the_backdrop() {
        let mut frame = Frame::new(2, 2);
        frame.clear([0, 100, 200]);
        // Half-transparent white, premultiplied as the protocol requires: 128
        // of each channel with alpha 128. The backdrop must come
        // through at half strength.
        let bytes = argb(&[[128, 128, 128, 128]; 1], 1, 1);
        frame.draw(
            &image(&bytes, 1, 1),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 1, 1),
        );
        let pixel = sample(&frame, 0, 0).unwrap();
        assert_eq!(pixel[3], 255, "the frame stays opaque");
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
        assert_eq!(sample(&frame, 0, 0), Some([9, 8, 7, 255]));
    }

    #[test]
    fn drawing_is_clipped_to_the_frame() {
        let mut frame = Frame::new(4, 4);
        frame.clear([0, 0, 0]);
        let bytes = argb(&[[255, 255, 255, 255]; 4], 2, 2);
        // Straddle the left and top edges: only the in-frame quarter may be
        // drawn.
        frame.draw(
            &image(&bytes, 2, 2),
            Rect::new(0, 0, 2, 2),
            Rect::new(-1, -1, 2, 2),
        );
        assert_eq!(sample(&frame, 0, 0), Some([255, 255, 255, 255]));
        assert_eq!(sample(&frame, 1, 0), Some([0, 0, 0, 255]));
        assert_eq!(sample(&frame, 0, 1), Some([0, 0, 0, 255]));
    }

    #[test]
    fn scaling_uses_the_source_region() {
        let mut frame = Frame::new(4, 4);
        frame.clear([0, 0, 0]);
        // Two by two image, only the bottom-right pixel is white, shown at 4x4.
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
        assert_eq!(sample(&frame, 0, 0), Some([0, 0, 0, 255]));
        assert_eq!(sample(&frame, 2, 2), Some([255, 255, 255, 255]));
        assert_eq!(sample(&frame, 3, 3), Some([255, 255, 255, 255]));
        assert_eq!(rgba(&frame_image, 1, 1), [255, 255, 255, 255]);
    }

    #[test]
    fn tiles_only_report_changed_regions() {
        let mut frame = Frame::new(64, 64);
        frame.clear([0, 0, 0]);
        let mut tiles = Tiles::new(&frame, (32, 32));
        assert_eq!(
            tiles.diff(&frame).len(),
            4,
            "the first diff is a full repaint"
        );
        assert!(
            tiles.diff(&frame).is_empty(),
            "an unchanged frame is not sent"
        );
        let red = argb(&[[255, 0, 0, 255]; 4], 2, 2);
        frame.draw(
            &image(&red, 2, 2),
            Rect::new(0, 0, 2, 2),
            Rect::new(40, 40, 2, 2),
        );
        assert_eq!(
            tiles.diff(&frame),
            vec![Rect::new(32, 32, 32, 32)],
            "only the tile holding the change"
        );
        assert_eq!(tiles.diff(&frame), Vec::<Rect>::new());
    }

    #[test]
    fn a_resized_frame_reports_every_tile() {
        let mut frame = Frame::new(64, 64);
        frame.clear([0, 0, 0]);
        let mut tiles = Tiles::new(&frame, (32, 32));
        let _ = tiles.diff(&frame);
        frame.resize(32, 32);
        tiles = Tiles::new(&frame, (32, 32));
        assert_eq!(tiles.diff(&frame).len(), 1);
    }
}
