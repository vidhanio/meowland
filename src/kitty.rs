//! Encoding of the [kitty graphics protocol][spec].
//!
//! Everything the compositor needs to put pixels on the terminal screen:
//! transmitting raw RGBA image data (zlib compressed) and placing it into a
//! cell rectangle in one command, deleting images again, and the handful of
//! terminal modes the protocol relies on.
//!
//! This module only ever *writes* escapes. Reading the terminal's answers lives
//! in [`crate::tty`], which is the only place allowed to touch stdin.
//!
//! [spec]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::io::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use flate2::{Compression, write::ZlibEncoder};
use smithay::input::pointer::CursorIcon;

/// Maximum size of a base64 plot line, mandated by the protocol.
const CHUNK: usize = 4096;

/// Z-index of the composited screen. Positive values draw above the terminal
/// text, so window content covers whatever the terminal last wrote in those
/// cells.
const Z_ABOVE_TEXT: i32 = 1;
/// Reusable compression and base64 storage for tile transmissions.
#[derive(Debug)]
pub struct Encoder {
    zlib: ZlibEncoder<Vec<u8>>,
    payload: Vec<u8>,
    finished: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Self {
            zlib: ZlibEncoder::new(Vec::new(), Compression::fast()),
            payload: Vec::new(),
            finished: false,
        }
    }
}

/// Image identity and terminal-cell placement for one transmission.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
}

/// Start a synchronized update. The terminal buffers everything until
/// [`end_sync`], which is what keeps a frame from tearing.
pub fn begin_sync(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026h");
}

/// End a synchronized update, making the buffered frame visible.
pub fn end_sync(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026l");
}

/// Set the terminal's mouse pointer shape, or reset it to the terminal's own
/// default.
///
/// This is the one part of the pointer the terminal draws better than we can:
/// its pointer is a real pointer, and clients usually say what they want shown
/// (a text beam, a resize arrow, a hand over a link) through
/// `wp_cursor_shape_manager_v1`.
pub fn set_pointer_shape(out: &mut Vec<u8>, shape: Option<&str>) {
    match shape {
        Some(shape) => {
            let _ = write!(out, "\x1b]22;{shape}\x1b\\");
        }
        None => out.extend_from_slice(b"\x1b]22;\x1b\\"),
    }
}

/// The [pointer shape name][names] that stands for a Wayland cursor icon.
///
/// [names]: https://sw.kovidgoyal.net/kitty/pointer-shapes/#pointer-shape-names
pub const fn pointer_shape(icon: CursorIcon) -> &'static str {
    match icon {
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize | CursorIcon::ColResize => "ew-resize",
        CursorIcon::NsResize | CursorIcon::RowResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        // The terminal's set of shapes has no name for these; the four-way arrow is the closest
        // thing it does have.
        CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::AllResize => "move",
        _ => "default",
    }
}

/// Move the cursor to a cell. Coordinates are 1-based, like the escape code it
/// turns into.
pub fn cursor_to(out: &mut Vec<u8>, col: u32, row: u32) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

/// Delete every image, freeing the terminal's memory for them.
pub fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
}

/// Transmit `pixels` as the image `id`, and place it in the cell rectangle that
/// starts at the cursor.
///
/// Doing both in one command is what makes an update cheap and flicker free: an
/// image id is replaced atomically (the old placement disappears with the old
/// data), so a tile can be re-sent while it is on screen. `C=1` keeps the
/// cursor where it is, so placing an image never scrolls the terminal and never
/// moves the anchor the next tile is addressed from.
impl Encoder {
    /// Compress and transmit `pixels` as image `id`, placing it at the cursor.
    pub fn transmit_and_place(&mut self, out: &mut Vec<u8>, pixels: &[u8], placement: Placement) {
        let Placement {
            id,
            width,
            height,
            cols,
            rows,
        } = placement;
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        if self.finished {
            let mut compressed = self
                .zlib
                .reset(Vec::new())
                .expect("resetting a Vec encoder cannot fail");
            compressed.clear();
            *self.zlib.get_mut() = compressed;
            self.finished = false;
        }
        self.zlib.get_mut().reserve(pixels.len() / 8);

        // Compositor output is mostly flat color, so compressing it typically
        // shrinks a tile by an order of magnitude; that ratio is what keeps the
        // pty from becoming the bottleneck.
        self.zlib
            .write_all(pixels)
            .expect("writing to a Vec cannot fail");
        self.zlib.try_finish().expect("finishing a Vec cannot fail");
        self.finished = true;

        let compressed = self.zlib.get_ref();
        let encoded_len =
            base64::encoded_len(compressed.len(), true).expect("a tile fits in address space");
        self.payload.resize(encoded_len, 0);
        let payload_len = BASE64
            .encode_slice(compressed, &mut self.payload)
            .expect("the payload buffer has the exact encoded size");
        chunked(
            out,
            id,
            width,
            height,
            cols,
            rows,
            &self.payload[..payload_len],
        );
    }
}

fn chunked(
    out: &mut Vec<u8>,
    id: u32,
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    payload: &[u8],
) {
    let mut chunks = payload.chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        out.extend_from_slice(b"\x1b_G");
        // Only the first escape of a multi-part transmission carries the
        // control data.
        if first {
            let _ = write!(
                out,
                "a=T,f=32,o=z,s={width},v={height},i={id},p={id},c={cols},r={rows},C=1,z={Z_ABOVE_TEXT},q=2,"
            );
            first = false;
        }
        out.extend_from_slice(if chunks.peek().is_some() {
            b"m=1;"
        } else {
            b"m=0;"
        });
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoding of one escape sequence, following the protocol by the book:
    /// this is the independent side of the round trip, so a wrong chunk
    /// size, format or byte order fails.
    #[derive(Debug, PartialEq, Eq)]
    enum Command {
        Transmit {
            id: u32,
            width: u32,
            height: u32,
            compressed: bool,
            pixels: Vec<u8>,
        },
        Put {
            id: u32,
            placement: u32,
            cols: u32,
            rows: u32,
            moves_cursor: bool,
        },
        DeleteAll,
    }

    fn decode(stream: &[u8]) -> Vec<Command> {
        let text = std::str::from_utf8(stream).expect("escapes are ASCII");
        let mut commands = Vec::new();
        let mut rest = text;
        // Partial transmissions are reassembled before being interpreted: the
        // header comes from the first chunk, the payload from all of
        // them.
        let mut pending: Option<(String, String)> = None;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").expect("unterminated escape");
            let (control, payload) = body[..end].split_once(';').unwrap_or((&body[..end], ""));
            assert!(payload.len() <= CHUNK, "chunk exceeds the protocol limit");
            rest = &body[end + 2..];

            let (header, data) = match pending.take() {
                Some((header, mut data)) => {
                    data.push_str(payload);
                    (header, data)
                }
                None => (control.to_owned(), payload.to_owned()),
            };
            if control.contains("m=1") {
                pending = Some((header, data));
            } else {
                commands.extend(interpret(&header, &data));
            }
        }
        assert!(pending.is_none(), "stream ends mid-transmission");
        commands
    }

    fn field<'a>(header: &'a str, key: &str) -> Option<&'a str> {
        header
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    fn number(header: &str, key: &str, default: u32) -> u32 {
        field(header, key).map_or(default, |v| v.parse().unwrap())
    }

    /// Interpret one reassembled graphics command. A command can transmit *and*
    /// place, so this returns a list.
    fn interpret(header: &str, payload: &str) -> Vec<Command> {
        let action = field(header, "a");
        let mut commands = Vec::new();
        if matches!(action, Some("t" | "T")) {
            assert_eq!(field(header, "f"), Some("32"), "we only send RGBA");
            let compressed = field(header, "o") == Some("z");
            let (width, height) = (number(header, "s", 0), number(header, "v", 0));
            let raw = BASE64
                .decode(payload)
                .expect("payload must be valid base64");
            let pixels = if compressed {
                zlib_decompress(&raw)
            } else {
                raw
            };
            assert_eq!(
                pixels.len(),
                width as usize * height as usize * 4,
                "pixel data must match the declared geometry"
            );
            commands.push(Command::Transmit {
                id: number(header, "i", 0),
                width,
                height,
                compressed,
                pixels,
            });
        }
        if matches!(action, Some("p" | "T")) {
            commands.push(Command::Put {
                id: number(header, "i", 0),
                placement: number(header, "p", 0),
                cols: number(header, "c", 0),
                rows: number(header, "r", 0),
                moves_cursor: field(header, "C") != Some("1"),
            });
        }
        if action == Some("d") {
            commands.push(Command::DeleteAll);
        }
        assert!(!commands.is_empty(), "unexpected action {action:?}");
        commands
    }

    fn zlib_decompress(data: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut out)
            .expect("payload must be a zlib stream");
        out
    }

    fn test_pixels(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
            }
        }
        pixels
    }

    #[test]
    fn a_tile_round_trips_pixels_exactly() {
        // Large enough to need several chunks, so chunk reassembly is covered
        // too.
        let (width, height) = (64, 48);
        let pixels = test_pixels(width, height);
        let mut out = Vec::new();
        Encoder::default().transmit_and_place(
            &mut out,
            &pixels,
            Placement {
                id: 7,
                width,
                height,
                cols: 6,
                rows: 4,
            },
        );

        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id: 7,
                    width,
                    height,
                    compressed: true,
                    pixels,
                },
                Command::Put {
                    id: 7,
                    placement: 7,
                    cols: 6,
                    rows: 4,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn an_encoder_can_be_reused() {
        let mut encoder = Encoder::default();
        let mut out = Vec::new();
        encoder.transmit_and_place(
            &mut out,
            &[1, 2, 3, 255],
            Placement {
                id: 1,
                width: 1,
                height: 1,
                cols: 1,
                rows: 1,
            },
        );
        out.clear();
        encoder.transmit_and_place(
            &mut out,
            &[4, 5, 6, 255],
            Placement {
                id: 2,
                width: 1,
                height: 1,
                cols: 1,
                rows: 1,
            },
        );

        assert_eq!(
            decode(&out),
            vec![
                Command::Transmit {
                    id: 2,
                    width: 1,
                    height: 1,
                    compressed: true,
                    pixels: vec![4, 5, 6, 255],
                },
                Command::Put {
                    id: 2,
                    placement: 2,
                    cols: 1,
                    rows: 1,
                    moves_cursor: false,
                },
            ]
        );
    }

    #[test]
    fn every_chunk_is_a_whole_base64_line() {
        // Noise, so zlib cannot shrink the payload below one chunk.
        let (width, height) = (256, 256);
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        let mut state = 0x1234_5678u32;
        for _ in 0..width * height {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.extend_from_slice(&state.to_le_bytes());
        }
        let mut out = Vec::new();
        Encoder::default().transmit_and_place(
            &mut out,
            &pixels,
            Placement {
                id: 1,
                width,
                height,
                cols: 20,
                rows: 10,
            },
        );

        let mut rest = std::str::from_utf8(&out).unwrap();
        let mut chunks = 0;
        while let Some(start) = rest.find("\x1b_G") {
            let body = &rest[start + 3..];
            let end = body.find("\x1b\\").unwrap();
            let (control, payload) = body[..end].split_once(';').unwrap();
            assert!(payload.len() <= CHUNK, "chunk longer than 4096 bytes");
            assert_eq!(payload.len() % 4, 0, "chunks must be whole base64 quanta");
            if control.contains("m=1") {
                // Anything followed by more data has to fill a whole chunk, or
                // the terminal would be handed a payload it
                // cannot decode when it concatenates.
                assert_eq!(
                    payload.len(),
                    CHUNK,
                    "a chunk with a successor must be full"
                );
            }
            chunks += 1;
            rest = &body[end + 2..];
        }
        assert!(chunks > 1, "this test needs a multi-chunk payload");
    }

    #[test]
    fn pointer_shapes_use_the_names_the_terminal_knows() {
        assert_eq!(pointer_shape(CursorIcon::Default), "default");
        assert_eq!(pointer_shape(CursorIcon::Text), "text");
        assert_eq!(pointer_shape(CursorIcon::Pointer), "pointer");
        // Icons the terminal set has no separate name for share the nearest
        // one.
        assert_eq!(pointer_shape(CursorIcon::ColResize), "ew-resize");
        assert_eq!(pointer_shape(CursorIcon::RowResize), "ns-resize");
        assert_eq!(pointer_shape(CursorIcon::AllScroll), "move");
        assert_eq!(pointer_shape(CursorIcon::ContextMenu), "default");
    }

    #[test]
    fn setting_a_pointer_shape_is_one_escape() {
        let mut out = Vec::new();
        set_pointer_shape(&mut out, Some("text"));
        set_pointer_shape(&mut out, None);
        assert_eq!(out, b"\x1b]22;text\x1b\\\x1b]22;\x1b\\");
    }

    #[test]
    fn deleting_everything_is_a_single_escape() {
        let mut out = Vec::new();
        delete_all(&mut out);
        assert_eq!(decode(&out), vec![Command::DeleteAll]);
    }

    #[test]
    fn tilde_the_cursor_is_addressed_one_based() {
        let mut out = Vec::new();
        cursor_to(&mut out, 0, 0);
        cursor_to(&mut out, 3, 11);
        assert_eq!(out, b"\x1b[1;1H\x1b[12;4H");
    }
}
