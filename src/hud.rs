//! A frame-rate counter, drawn into the top left corner of the frame.
//!
//! The terminal draws no text for us: everything on screen is pixels the
//! compositor put there (see [`crate::render`]), and the window covers the
//! whole grid, so a counter has to be pixels too - terminal text would sit
//! behind the window rather than over it.
//!
//! Three rows of bits per glyph is enough to read a number once it is scaled
//! up, and the whole table is a few hundred bytes.

use crate::render::{Frame, Rect};

/// How much one bit of a glyph is worth, in pixels. Sized so the counter is
/// about as tall as a character cell.
const SCALE: u32 = 4;

/// Distance from the corner, in pixels.
const MARGIN: i32 = 8;

/// Space around the glyphs, so the counter is readable over anything.
const PADDING: u32 = 4;

/// The counter's own colours: near-black behind, light in front.
const BACKGROUND: [u8; 3] = [0x0a, 0x0b, 0x10];
const FOREGROUND: [u8; 3] = [0xd8, 0xdc, 0xe4];

/// The glyphs: five rows of three bits each, left to right.
const fn glyph(character: char) -> [u8; 5] {
    match character {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        // What the counter's own text needs, and nothing else: a glyph that
        // nothing draws is a glyph nobody has checked.
        'f' => [0b111, 0b100, 0b111, 0b100, 0b100],
        'p' => [0b111, 0b101, 0b111, 0b100, 0b100],
        's' => [0b011, 0b100, 0b010, 0b001, 0b110],
        // Anything else - a space, say - is left blank.
        _ => [0, 0, 0, 0, 0],
    }
}

/// Draw `text` in the corner of `frame`, over whatever is there.
pub fn draw(frame: &mut Frame, text: &str) {
    const GLYPH: u32 = 3;
    /// One empty column between glyphs.
    const SPACING: u32 = 1;

    let characters = text.chars().count() as u32;
    if characters == 0 {
        return;
    }
    let width = (characters * (GLYPH + SPACING) - SPACING) * SCALE;
    let height = 5 * SCALE;
    frame.fill(
        Rect::new(
            MARGIN - PADDING as i32,
            MARGIN - PADDING as i32,
            width + PADDING * 2,
            height + PADDING * 2,
        ),
        BACKGROUND,
    );

    for (index, character) in text.chars().enumerate() {
        let rows = glyph(character);
        let left = MARGIN + index as i32 * (GLYPH as i32 + SPACING as i32) * SCALE as i32;
        for (row, bits) in rows.iter().enumerate() {
            for column in 0..GLYPH {
                // The high bit of the row is the leftmost pixel of the glyph.
                if bits & (1 << (GLYPH - 1 - column)) == 0 {
                    continue;
                }
                frame.fill(
                    Rect::new(
                        left + column as i32 * SCALE as i32,
                        MARGIN + row as i32 * SCALE as i32,
                        SCALE,
                        SCALE,
                    ),
                    FOREGROUND,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The glyphs `text` leaves on the frame, sampled on the grid the font
    /// defines - the same three bits a row, five rows a character, that an eye
    /// reads off the screen.
    fn read_back(frame: &Frame, characters: usize) -> Vec<[u8; 5]> {
        let width = frame.bounds().width;
        let mut glyphs = Vec::new();
        for index in 0..characters {
            let left = MARGIN as u32 + index as u32 * 16;
            let mut rows = [0u8; 5];
            for (row, bits) in rows.iter_mut().enumerate() {
                for column in 0..3 {
                    let x = left + column * SCALE + SCALE / 2;
                    let y = MARGIN as u32 + row as u32 * SCALE + SCALE / 2;
                    let pixel = (y * width + x) as usize * crate::render::BYTES;
                    // Light pixels are glyph, dark ones are the counter's own
                    // background; everything around it is the client's.
                    if frame.pixels()[pixel] > 0x80 {
                        *bits |= 1 << (2 - column);
                    }
                }
            }
            glyphs.push(rows);
        }
        glyphs
    }

    #[test]
    fn the_counter_reads_back_off_the_frame() {
        let mut frame = Frame::new(240, 40);
        draw(&mut frame, "23 fps");
        let expected: Vec<[u8; 5]> = "23 fps".chars().map(glyph).collect();
        assert_eq!(read_back(&frame, expected.len()), expected);
    }

    #[test]
    fn a_blank_glyph_is_a_space() {
        assert_eq!(glyph(' '), [0; 5]);
    }
}
