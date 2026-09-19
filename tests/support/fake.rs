//! A simulated kitty-compatible terminal for the pane tests.
//!
//! It answers the pane's queries, decodes the kitty graphics escapes the pane
//! produces and keeps the resulting screen, so a test can compare pixels after
//! any sequence of whole frames and patches.  The decoder is written directly
//! from the protocol description and never shares code with the encoder.

use std::collections::HashSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};

#[derive(Debug)]
struct Image {
    id: u32,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

pub struct FakeTerminal {
    pub width: usize,
    pub height: usize,
    pub cell: (u16, u16),
    pub modes: HashSet<String>,
    pub synchronized_updates: u32,
    /// How many whole frames and how many patches the pane sent, and how many
    /// of the whole frames arrived through a shared memory object.
    pub whole_frames: u32,
    pub patches: u32,
    pub shared_frames: u32,
    /// Plain (non-escape) output, which is where error messages land.
    pub text: Vec<u8>,
    pending: Vec<u8>,
    base: Vec<u8>,
    screen: Vec<u8>,
    images: Vec<Image>,
    cursor: (usize, usize),
    chunk: Option<(u32, bool, usize, usize, bool, Vec<u8>)>,
}

impl FakeTerminal {
    pub fn new(width: usize, height: usize, cell: (u16, u16)) -> Self {
        let base = vec![0; width * height * 3];
        Self {
            width,
            height,
            cell,
            modes: HashSet::new(),
            synchronized_updates: 0,
            whole_frames: 0,
            patches: 0,
            shared_frames: 0,
            text: Vec::new(),
            pending: Vec::new(),
            screen: base.clone(),
            base,
            images: Vec::new(),
            cursor: (0, 0),
            chunk: None,
        }
    }

    pub fn screen(&self) -> &[u8] {
        &self.screen
    }

    /// Consume whatever the pane wrote and return the replies a real terminal
    /// would send back.
    pub fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(input);
        let mut replies = Vec::new();
        loop {
            let Some(escape) = self.pending.iter().position(|byte| *byte == 0x1b) else {
                self.text.append(&mut self.pending);
                break;
            };
            if escape > 0 {
                self.text.extend_from_slice(&self.pending[..escape]);
                self.pending.drain(..escape);
            }
            if self.pending.len() < 2 {
                break;
            }
            match self.pending[1] {
                b'[' => {
                    let Some(end) = self.pending[2..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                        .map(|offset| offset + 2)
                    else {
                        break;
                    };
                    let params = String::from_utf8_lossy(&self.pending[2..end]).into_owned();
                    let final_byte = self.pending[end];
                    self.pending.drain(..=end);
                    self.csi(&params, final_byte, &mut replies);
                }
                b'_' => {
                    let Some(end) = self.pending[3..]
                        .windows(2)
                        .position(|window| window == b"\x1b\\")
                        .map(|offset| offset + 3)
                    else {
                        break;
                    };
                    let body = self.pending[3..end].to_vec();
                    self.pending.drain(..end + 2);
                    self.apc(&body, &mut replies);
                }
                b']' => {
                    let end = self.pending[2..]
                        .iter()
                        .position(|byte| *byte == 0x07 || *byte == 0x1b);
                    match end {
                        Some(offset) => {
                            let end = if self.pending[2 + offset] == 0x07 {
                                2 + offset + 1
                            } else {
                                2 + offset + 2
                            };
                            self.pending.drain(..end);
                        }
                        None => break,
                    }
                }
                _ => {
                    self.pending.drain(..2);
                }
            }
        }
        replies
    }

    fn csi(&mut self, params: &str, final_byte: u8, replies: &mut Vec<u8>) {
        let (cell_width, cell_height) = (self.cell.0 as usize, self.cell.1 as usize);
        match final_byte {
            b'H' => {
                if params.is_empty() {
                    self.cursor = (0, 0);
                } else {
                    let mut parts = params.split(';');
                    let row = parts
                        .next()
                        .and_then(|p| p.parse::<usize>().ok())
                        .unwrap_or(1);
                    let col = parts
                        .next()
                        .and_then(|p| p.parse::<usize>().ok())
                        .unwrap_or(1);
                    self.cursor = (row.saturating_sub(1), col.saturating_sub(1));
                }
            }
            b't' => match params {
                "14" => replies.extend_from_slice(
                    format!("\x1b[4;{};{}t", self.height as u32, self.width as u32).as_bytes(),
                ),
                "16" => replies
                    .extend_from_slice(format!("\x1b[6;{cell_height};{cell_width}t").as_bytes()),
                _ => {}
            },
            b'c' => replies.extend_from_slice(b"\x1b[?62;4;6;22c"),
            b'p' => match params {
                "?1016$" => replies.extend_from_slice(b"\x1b[?1016;2$y"),
                "?2026$" => replies.extend_from_slice(b"\x1b[?2026;2$y"),
                "?2027$" => replies.extend_from_slice(b"\x1b[?2027;2$y"),
                _ => {}
            },
            b'u' => {
                if params.starts_with('?') {
                    // No keyboard enhancement: the plain key path is easier to
                    // drive deterministically from a test.
                    replies.extend_from_slice(b"\x1b[?0u");
                }
            }
            b'h' | b'l' => {
                for mode in params.trim_start_matches('?').split(';') {
                    let key = format!("{mode}{}", final_byte as char);
                    if final_byte == b'h' {
                        if mode == "2026" {
                            self.synchronized_updates += 1;
                        }
                        self.modes.insert(key);
                    } else {
                        self.modes.remove(&key);
                    }
                }
            }
            _ => {}
        }
    }

    fn apc(&mut self, body: &[u8], replies: &mut Vec<u8>) {
        let (control, payload) = body
            .iter()
            .position(|byte| *byte == b';')
            .map_or((body, &[][..]), |index| {
                (&body[..index], &body[index + 1..])
            });
        let control = String::from_utf8_lossy(control).into_owned();
        let fields: std::collections::HashMap<&str, &str> = control
            .split(',')
            .filter_map(|field| field.split_once('='))
            .collect();
        let id = fields.get("i").and_then(|id| id.parse::<u32>().ok());
        match fields.get("a").copied() {
            Some("q") => {
                if let Some(id) = id {
                    let answer = if fields.get("t").copied() == Some("s") {
                        // A terminal that cannot read the object answers with
                        // an error, and the pane then keeps to the pty.
                        if self.shared(&fields, payload) {
                            "OK"
                        } else {
                            "ENOTSUP"
                        }
                    } else {
                        "OK"
                    };
                    replies.extend_from_slice(format!("\x1b_Gi={id};{answer}\x1b\\").as_bytes());
                }
            }
            Some("t") if fields.get("q").copied() != Some("2") => {
                if let Some(id) = id {
                    replies.extend_from_slice(format!("\x1b_Gi={id};OK\x1b\\").as_bytes());
                }
            }
            Some("d") => {
                match fields.get("d").copied() {
                    Some("A") => self.images.clear(),
                    Some("I") => {
                        if let Some(id) = id {
                            self.images.retain(|image| image.id != id);
                        }
                    }
                    _ => {}
                }
                self.recompose();
            }
            Some("T") => {
                if fields.get("t").copied() == Some("s") {
                    self.shared(&fields, payload);
                } else {
                    self.graphics(&fields, payload);
                }
            }
            _ => {}
        }
    }

    /// The `t=s` form: the pixels sit in a POSIX shared memory object whose
    /// name is the payload.  A terminal reads and unlinks it, and a missing
    /// object is how it says it cannot read one.
    fn shared(&mut self, fields: &std::collections::HashMap<&str, &str>, payload: &[u8]) -> bool {
        let Ok(name) = STANDARD.decode(payload) else {
            return false;
        };
        let Ok(name) = String::from_utf8(name) else {
            return false;
        };
        let Ok(pixels) = std::fs::read(format!("/dev/shm{name}")) else {
            return false;
        };
        let _ = std::fs::remove_file(format!("/dev/shm{name}"));
        if fields.get("a").copied() == Some("T") {
            self.shared_frames += 1;
            let id = fields["i"].parse().unwrap();
            let width = fields["s"].parse().unwrap();
            let height = fields["v"].parse().unwrap();
            let patch = fields.get("p").copied() == Some("1");
            self.place_pixels(id, patch, width, height, pixels);
        }
        true
    }

    fn graphics(&mut self, fields: &std::collections::HashMap<&str, &str>, payload: &[u8]) {
        let more = fields.get("m") == Some(&"1");
        match self.chunk.as_mut() {
            Some(chunk) if !fields.contains_key("a") => {
                chunk.5.extend_from_slice(payload);
                if !more {
                    let chunk = self.chunk.take().unwrap();
                    self.place(chunk);
                }
            }
            _ => {
                let id = fields["i"].parse().unwrap();
                let width = fields["s"].parse().unwrap();
                let height = fields["v"].parse().unwrap();
                let patch = fields.get("p").copied() == Some("1");
                let compressed = fields.get("o").copied() == Some("z");
                let data = payload.to_vec();
                let chunk = (id, patch, width, height, compressed, data);
                if more {
                    self.chunk = Some(chunk);
                } else {
                    self.place(chunk);
                }
            }
        }
    }

    fn place(
        &mut self,
        (id, patch, width, height, compressed, data): (u32, bool, usize, usize, bool, Vec<u8>),
    ) {
        let mut decoded = STANDARD.decode(data).expect("base64 payload");
        if compressed {
            let mut zlib = flate2::read::ZlibDecoder::new(decoded.as_slice());
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut zlib, &mut out).unwrap();
            decoded = out;
        }
        self.place_pixels(id, patch, width, height, decoded);
    }

    fn place_pixels(&mut self, id: u32, patch: bool, width: usize, height: usize, pixels: Vec<u8>) {
        assert_eq!(pixels.len(), width * height * 3, "payload size");
        let (cell_width, cell_height) = (self.cell.0 as usize, self.cell.1 as usize);
        if patch {
            self.patches += 1;
            let image = Image {
                id,
                x: self.cursor.0 * cell_width,
                y: self.cursor.1 * cell_height,
                width,
                height,
                pixels,
            };
            self.images.retain(|image| image.id != id);
            self.images.push(image);
        } else {
            self.whole_frames += 1;
            self.base = pixels;
            self.images.clear();
        }
        self.recompose();
    }

    fn recompose(&mut self) {
        self.screen.copy_from_slice(&self.base);
        for image in &self.images {
            for row in 0..image.height {
                let destination = ((image.y + row) * self.width + image.x) * 3;
                let source = row * image.width * 3;
                self.screen[destination..destination + image.width * 3]
                    .copy_from_slice(&image.pixels[source..source + image.width * 3]);
            }
        }
    }
}
