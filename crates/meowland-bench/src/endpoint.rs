//! Headless terminal for a pane: answers its capability probe and rebuilds
//! the Kitty graphics it writes into RGB images.
//!
//! Supported transmissions are whole images and in-place frame edits of the
//! root image, sent either through POSIX shared memory (`t=s`) or inline
//! (`t=d`), optionally zlib-compressed (`o=z`) and chunked (`m=1`). Anything
//! else is an error, so an unexpected command fails the run instead of being
//! ignored. A frame is consumed when synchronized output ends with a valid
//! marker; its pixels are then probed with [`pattern::verify`].

use std::{
    fs::{self, File},
    io::{self, Read as _},
    ops::Sub,
    path::Path,
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status};

use crate::{
    control,
    pattern::{self, MARKER_PIXELS, Pattern},
};

const ESC: u8 = 0x1b;
/// Longest escape sequence held while waiting for its terminator.
const MAX_PENDING: usize = 64 * 1024;
/// Image ID of the pane's frame-edit probe.
const PROBE_ID: &str = "30";
/// Image ID of the pane's screen image.
const SCREEN_ID: &str = "1";

/// Image transports the endpoint accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Shared memory and inline data.
    Shared,
    /// Inline data only; shared-memory queries are refused.
    Inline,
}

/// Running totals of the terminal output consumed so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Bytes read from the pane.
    pub output_bytes: u64,
    /// Images or edits transmitted through shared memory.
    pub shared_uploads: u64,
    /// Images or edits transmitted inline.
    pub inline_uploads: u64,
    /// Whole-image transmissions.
    pub whole_uploads: u64,
    /// In-place frame edits.
    pub edit_uploads: u64,
    /// Decoded RGB bytes across all transmissions.
    pub pixel_bytes: u64,
}

impl Sub for Counters {
    type Output = Self;

    fn sub(self, earlier: Self) -> Self {
        Self {
            output_bytes: self.output_bytes - earlier.output_bytes,
            shared_uploads: self.shared_uploads - earlier.shared_uploads,
            inline_uploads: self.inline_uploads - earlier.inline_uploads,
            whole_uploads: self.whole_uploads - earlier.whole_uploads,
            edit_uploads: self.edit_uploads - earlier.edit_uploads,
            pixel_bytes: self.pixel_bytes - earlier.pixel_bytes,
        }
    }
}

/// A frame the endpoint consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Workload frame ID from the image's marker.
    pub id: u64,
    /// When validation finished, from [`control::now`].
    pub at: Duration,
}

/// Control keys of one graphics command, indexed by their ASCII letter.
struct Fields<'a> {
    header: &'a str,
    values: [Option<&'a str>; 128],
}

impl<'a> Fields<'a> {
    fn parse(header: &'a [u8]) -> anyhow::Result<Self> {
        let header = std::str::from_utf8(header).context("non-ASCII graphics command")?;
        let mut values = [None; 128];
        for pair in header.split(',').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .with_context(|| format!("malformed graphics field {pair:?}"))?;
            let slot = match key.as_bytes() {
                &[key] => values.get_mut(usize::from(key)),
                _ => None,
            };
            *slot.with_context(|| format!("unsupported graphics key {key:?}"))? = Some(value);
        }
        Ok(Self { header, values })
    }

    fn get(&self, key: u8) -> Option<&'a str> {
        self.values[usize::from(key)]
    }

    fn is(&self, key: u8, value: &str) -> bool {
        self.get(key) == Some(value)
    }

    fn number(&self, key: u8) -> anyhow::Result<usize> {
        self.get(key).map_or(Ok(0), |value| {
            value
                .parse()
                .with_context(|| format!("invalid graphics number {value:?}"))
        })
    }
}

/// A transmission whose chunks are still arriving.
struct Transfer {
    edit: bool,
    shared: bool,
    compressed: bool,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Transfer {
    const fn length(&self) -> usize {
        self.width * self.height * 3
    }
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Transfer")
            .field("edit", &self.edit)
            .field("shared", &self.shared)
            .field("size", &(self.width, self.height))
            .field("received", &self.data.len())
            .finish_non_exhaustive()
    }
}

/// A headless Kitty terminal of a fixed pixel size showing one workload.
#[derive(Debug)]
pub struct Endpoint {
    width: usize,
    height: usize,
    pattern: Pattern,
    transport: Transport,
    pending: Vec<u8>,
    image: Option<Vec<u8>>,
    transfer: Option<Transfer>,
    last: Option<u64>,
    probing: bool,
    counters: Counters,
}

impl Endpoint {
    /// A terminal `width`×`height` pixels showing `pattern`, accepting
    /// `transport`.
    pub fn new(width: usize, height: usize, pattern: Pattern, transport: Transport) -> Self {
        Self {
            width,
            height,
            pattern,
            transport,
            pending: Vec::new(),
            image: None,
            transfer: None,
            last: None,
            probing: false,
            counters: Counters::default(),
        }
    }

    /// Totals so far.
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// Consume pane output, appending replies for the pane to `replies` and
    /// newly displayed frames to `frames`.
    ///
    /// # Errors
    /// Returns an error for unsupported or malformed output, or a frame whose
    /// pixels do not match its marker.
    pub fn feed(
        &mut self,
        data: &[u8],
        replies: &mut Vec<u8>,
        frames: &mut Vec<Frame>,
    ) -> anyhow::Result<()> {
        self.counters.output_bytes += u64::try_from(data.len())?;
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(data);
        let parsed = self.parse(&pending, replies, frames);
        let consumed = match parsed {
            Ok(consumed) => consumed,
            Err(error) => {
                self.pending = pending;
                return Err(error);
            }
        };
        pending.drain(..consumed);
        ensure!(
            pending.len() <= MAX_PENDING,
            "unterminated terminal escape sequence"
        );
        self.pending = pending;
        Ok(())
    }

    /// Handle every complete sequence in `bytes`, returning how many bytes
    /// were consumed.
    fn parse(
        &mut self,
        bytes: &[u8],
        replies: &mut Vec<u8>,
        frames: &mut Vec<Frame>,
    ) -> anyhow::Result<usize> {
        let mut at = 0;
        while let Some(rest) = bytes.get(at..).filter(|rest| !rest.is_empty()) {
            if rest[0] != ESC {
                at += rest
                    .iter()
                    .position(|&byte| byte == ESC)
                    .unwrap_or(rest.len());
                continue;
            }
            let Some(&kind) = rest.get(1) else {
                break;
            };
            match kind {
                b'_' | b']' => {
                    let Some(end) = rest[2..].windows(2).position(|pair| pair == [ESC, b'\\'])
                    else {
                        break;
                    };
                    let body = &rest[2..2 + end];
                    if kind == b'_' {
                        if let Some(command) = body.strip_prefix(b"G") {
                            self.graphics(command, replies)?;
                        }
                    } else if body.starts_with(b"52;") && body.ends_with(b";?") {
                        replies.extend_from_slice(b"\x1b]52;c;\x1b\\");
                    }
                    at += end + 4;
                }
                b'[' => {
                    let parameters = rest[2..]
                        .iter()
                        .position(|byte| !(0x30..=0x3f).contains(byte))
                        .map_or(rest.len(), |length| 2 + length);
                    let intermediates = rest[parameters..]
                        .iter()
                        .position(|byte| !(0x20..=0x2f).contains(byte))
                        .map_or(rest.len(), |length| parameters + length);
                    let Some(&last) = rest.get(intermediates) else {
                        break;
                    };
                    if (0x40..=0x7e).contains(&last) {
                        self.csi(&rest[2..parameters], last, replies, frames)?;
                        at += intermediates + 1;
                    } else {
                        at += 2;
                    }
                }
                _ => at += 2,
            }
        }
        Ok(at)
    }

    fn csi(
        &mut self,
        parameters: &[u8],
        last: u8,
        replies: &mut Vec<u8>,
        frames: &mut Vec<Frame>,
    ) -> anyhow::Result<()> {
        match (parameters, last) {
            (b"16", b't') => replies.extend_from_slice(b"\x1b[6;20;10t"),
            (b"14", b't') => {
                replies.extend_from_slice(
                    format!("\x1b[4;{};{}t", self.height, self.width).as_bytes(),
                );
            }
            (b"", b'c') => replies.extend_from_slice(b"\x1b[?1;2c"),
            (b">", b'c') => replies.extend_from_slice(b"\x1b[>1;1;0c"),
            (b"?", b'u') => replies.extend_from_slice(b"\x1b[?31u"),
            (b"?1016", b'p') => replies.extend_from_slice(b"\x1b[?1016;2$y"),
            (b"?5522", b'p') => replies.extend_from_slice(b"\x1b[?5522;2$y"),
            (b"?2026", b'l') => self.present(frames)?,
            _ => {}
        }
        Ok(())
    }

    /// Synchronized output ended: consume the image if it shows a new frame.
    fn present(&mut self, frames: &mut Vec<Frame>) -> anyhow::Result<()> {
        let Some(image) = &self.image else {
            return Ok(());
        };
        let id = pattern::identify(&image[..MARKER_PIXELS * 3]).context("corrupt frame marker")?;
        if self.last != Some(id) {
            pattern::verify(image, self.width, self.height, self.pattern, id)?;
            self.last = Some(id);
            frames.push(Frame {
                id,
                at: control::now(),
            });
        }
        Ok(())
    }

    fn graphics(&mut self, command: &[u8], replies: &mut Vec<u8>) -> anyhow::Result<()> {
        let (header, payload) = command
            .iter()
            .position(|&byte| byte == b';')
            .map_or((command, &[][..]), |split| {
                (&command[..split], &command[split + 1..])
            });
        let fields = Fields::parse(header)?;
        let action = fields.get(b'a');
        if action == Some("q") {
            let shared = fields.is(b't', "s");
            let supported = !shared || self.transport == Transport::Shared;
            if supported && shared {
                read_shared(payload, 3)?;
            }
            replies.extend_from_slice(
                format!(
                    "\x1b_Gi={};{}\x1b\\",
                    fields.get(b'i').unwrap_or("0"),
                    if supported { "OK" } else { "ENOTSUP" }
                )
                .as_bytes(),
            );
            return Ok(());
        }
        if fields.is(b'i', PROBE_ID) && fields.is(b's', "1") && fields.is(b'v', "1") {
            ensure!(
                STANDARD.decode(payload)? == [0; 3],
                "unexpected frame-edit probe content"
            );
            match action {
                Some("t") => self.probing = true,
                Some("f") if self.probing && fields.is(b'r', "1") => {
                    replies.extend_from_slice(b"\x1b_Gi=30;OK\x1b\\");
                }
                _ => bail!("unsupported frame-edit probe"),
            }
            return Ok(());
        }
        if action == Some("d") {
            if fields.is(b'd', "I") && fields.is(b'i', PROBE_ID) {
                self.probing = false;
            } else if fields.is(b'd', "A") || fields.is(b'i', SCREEN_ID) {
                self.image = None;
            } else {
                bail!("unsupported image deletion");
            }
            return Ok(());
        }
        let mut transfer = match self.transfer.take() {
            None => self.begin(&fields, action)?,
            Some(transfer) => {
                ensure!(
                    action.is_none() || (action == Some("f") && transfer.edit),
                    "interleaved Kitty transfers"
                );
                transfer
            }
        };
        let length = transfer.length();
        if fields.is(b't', "s") {
            let data = read_shared(payload, length)?;
            if transfer.data.is_empty() {
                transfer.data = data;
            } else {
                transfer.data.extend_from_slice(&data);
            }
        } else {
            STANDARD
                .decode_vec(payload, &mut transfer.data)
                .context("invalid base64 payload")?;
        }
        ensure!(
            transfer.data.len() <= length * 2 + 65536,
            "oversized Kitty transfer"
        );
        if fields.get(b'm').is_some_and(|more| more != "0") {
            self.transfer = Some(transfer);
            return Ok(());
        }
        self.finish(transfer)
    }

    fn begin(&self, fields: &Fields<'_>, action: Option<&str>) -> anyhow::Result<Transfer> {
        ensure!(
            matches!(action, Some("T" | "t" | "f")) && fields.is(b'f', "24"),
            "unsupported Kitty command {:?}",
            fields.header
        );
        let edit = action == Some("f");
        let (width, height) = (fields.number(b's')?, fields.number(b'v')?);
        ensure!(
            (1..=self.width).contains(&width) && (1..=self.height).contains(&height),
            "invalid image dimensions {width}x{height}"
        );
        ensure!(
            !edit || (fields.is(b'r', "1") && fields.is(b'X', "1")),
            "unsupported frame edit"
        );
        ensure!(
            fields.is(b'i', SCREEN_ID),
            "only the root image is supported"
        );
        Ok(Transfer {
            edit,
            shared: fields.is(b't', "s"),
            compressed: fields.is(b'o', "z"),
            x: fields.number(b'x')?,
            y: fields.number(b'y')?,
            width,
            height,
            data: Vec::new(),
        })
    }

    fn finish(&mut self, transfer: Transfer) -> anyhow::Result<()> {
        let length = transfer.length();
        let data = if transfer.compressed {
            inflate(&transfer.data, length)?
        } else {
            transfer.data
        };
        ensure!(data.len() == length, "incomplete Kitty image");
        if transfer.edit {
            let (x, y, width) = (transfer.x, transfer.y, transfer.width);
            ensure!(
                x + width <= self.width && y + transfer.height <= self.height,
                "frame edit exceeds its base image"
            );
            let image = self
                .image
                .as_mut()
                .context("frame edit without a base image")?;
            for (row, line) in data.chunks_exact(width * 3).enumerate() {
                let at = ((y + row) * self.width + x) * 3;
                image[at..at + width * 3].copy_from_slice(line);
            }
            self.counters.edit_uploads += 1;
        } else {
            ensure!(
                (transfer.width, transfer.height) == (self.width, self.height),
                "expected a whole {}x{} image, got {}x{}",
                self.width,
                self.height,
                transfer.width,
                transfer.height
            );
            self.image = Some(data);
            self.counters.whole_uploads += 1;
        }
        if transfer.shared {
            self.counters.shared_uploads += 1;
        } else {
            self.counters.inline_uploads += 1;
        }
        self.counters.pixel_bytes += u64::try_from(length)?;
        Ok(())
    }
}

/// Read and unlink the shared-memory object named by a base64 payload, as a
/// Kitty terminal does.
fn read_shared(payload: &[u8], length: usize) -> anyhow::Result<Vec<u8>> {
    let name = String::from_utf8(STANDARD.decode(payload)?)?;
    let suffix = name
        .strip_prefix("/meowland-")
        .filter(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .with_context(|| format!("invalid SHM name {name:?}"))?;
    let path = Path::new("/dev/shm").join(format!("meowland-{suffix}"));
    let mut file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let size = file.metadata()?.len();
    ensure!(
        size == u64::try_from(length)?,
        "SHM size {size} does not match the image's {length} bytes"
    );
    let mut data = Vec::with_capacity(length);
    file.read_to_end(&mut data)?;
    Ok(data)
}

fn inflate(compressed: &[u8], length: usize) -> anyhow::Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut data = Vec::with_capacity(length + 1);
    let status = decoder.decompress_vec(compressed, &mut data, FlushDecompress::Finish)?;
    ensure!(
        status == Status::StreamEnd && decoder.total_in() == u64::try_from(compressed.len())?,
        "invalid compressed image"
    );
    Ok(data)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::{Compression, write::ZlibEncoder};

    use super::*;

    fn image(pattern: Pattern, width: usize, height: usize, frame: u64) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| pattern.pixel(x, y, width, height, frame)))
            .collect()
    }

    fn chunked(control: &str, data: &[u8]) -> Vec<u8> {
        let encoded = STANDARD.encode(data);
        let chunks: Vec<_> = encoded.as_bytes().chunks(4096).collect();
        let mut out = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index + 1 < chunks.len());
            let prefix = if index == 0 {
                format!("{control},")
            } else {
                String::new()
            };
            out.extend_from_slice(format!("\x1b_G{prefix}m={more};").as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
        out
    }

    /// A compressed, chunked inline image split at arbitrary read
    /// boundaries must reassemble, and an edit must land at its offset.
    #[test]
    fn reassembles_split_chunks_and_edits() {
        let (width, height) = (320, 200);
        let mut endpoint = Endpoint::new(width, height, Pattern::Sparse, Transport::Inline);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder
            .write_all(&image(Pattern::Sparse, width, height, 0))
            .unwrap();
        let mut output = b"\x1b[?2026h".to_vec();
        output.extend(chunked(
            &format!("a=T,f=24,s={width},v={height},i=1,o=z"),
            &encoder.finish().unwrap(),
        ));
        output.extend_from_slice(b"\x1b[?2026l\x1b[?2026h");
        let next = image(Pattern::Sparse, width, height, 1);
        let (x, y, w, h) = (80, 100, 40, 20);
        let edit: Vec<u8> = (y..y + h)
            .flat_map(|row| next[(row * width + x) * 3..(row * width + x + w) * 3].to_vec())
            .collect();
        output.extend(chunked(
            &format!("a=f,f=24,s={w},v={h},x={x},y={y},i=1,r=1,X=1"),
            &edit,
        ));
        let marker: Vec<u8> = next[..MARKER_PIXELS * 3].to_vec();
        output.extend(chunked(
            &format!("a=f,f=24,s={MARKER_PIXELS},v=1,i=1,r=1,X=1"),
            &marker,
        ));
        output.extend_from_slice(b"\x1b[?2026l");

        let (mut replies, mut frames) = (Vec::new(), Vec::new());
        for piece in output.chunks(1000) {
            endpoint.feed(piece, &mut replies, &mut frames).unwrap();
        }
        assert_eq!(
            frames.iter().map(|frame| frame.id).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(endpoint.counters().edit_uploads, 2);
        assert_eq!(replies, b"");
    }

    /// The inline-only terminal must refuse shared memory, or the inline
    /// case would silently measure the shared path.
    #[test]
    fn inline_transport_refuses_shared_memory() {
        let mut endpoint = Endpoint::new(320, 200, Pattern::Full, Transport::Inline);
        let (mut replies, mut frames) = (Vec::new(), Vec::new());
        let query = format!(
            "\x1b_Ga=q,f=24,s=1,v=1,i=32,t=s;{}\x1b\\",
            STANDARD.encode("/meowland-absent")
        );
        endpoint
            .feed(query.as_bytes(), &mut replies, &mut frames)
            .unwrap();
        assert_eq!(replies, b"\x1b_Gi=32;ENOTSUP\x1b\\");
    }

    /// An image whose pixels disagree with its marker must fail, not count
    /// as a delivered frame.
    #[test]
    fn rejects_frames_with_wrong_pixels() {
        let (width, height) = (320, 200);
        let mut endpoint = Endpoint::new(width, height, Pattern::Full, Transport::Inline);
        let mut pixels = image(Pattern::Full, width, height, 2);
        pixels[..MARKER_PIXELS * 3]
            .copy_from_slice(&image(Pattern::Full, width, height, 3)[..MARKER_PIXELS * 3]);
        let mut output = chunked(&format!("a=T,f=24,s={width},v={height},i=1"), &pixels);
        output.extend_from_slice(b"\x1b[?2026l");
        let (mut replies, mut frames) = (Vec::new(), Vec::new());
        assert!(endpoint.feed(&output, &mut replies, &mut frames).is_err());
        assert_eq!(frames, []);
    }
}
