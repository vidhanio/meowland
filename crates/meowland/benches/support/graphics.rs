//! A bounded RGB Kitty consumer with chunking, zlib, SHM, placements, and
//! in-place root-frame edits.

use std::{collections::BTreeMap, fs::File, io::Read};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use rustix::{fs::Mode, shm};

use super::{
    config::{Size, Transport},
    fixture,
};
use crate::{Result, fail};

struct Image {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    z: usize,
    pixels: Vec<u8>,
}

struct Transfer {
    id: u32,
    image: Image,
    compressed: bool,
    transport: Transport,
    edit: bool,
    quiet: bool,
}

struct Control<'a>(BTreeMap<&'a str, &'a str>);

impl<'a> Control<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)?;
        let mut fields = BTreeMap::new();
        for field in text.split(',').filter(|field| !field.is_empty()) {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| fail("invalid Kitty control field"))?;
            fields.insert(key, value);
        }
        Ok(Self(fields))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).copied()
    }

    fn number(&self, key: &str, default: usize) -> Result<usize> {
        self.get(key)
            .map_or(Ok(default), |value| Ok(value.parse()?))
    }
}

pub(crate) struct Graphics {
    size: Size,
    transport: Transport,
    images: BTreeMap<u32, Image>,
    transfer: Option<Transfer>,
    pub cursor: (usize, usize),
    pub uploaded: Option<Transport>,
    last: Option<u64>,
}

impl Graphics {
    pub(crate) const fn new(size: Size, transport: Transport) -> Self {
        Self {
            size,
            transport,
            images: BTreeMap::new(),
            transfer: None,
            cursor: (0, 0),
            uploaded: None,
            last: None,
        }
    }

    fn read_shared(payload: &[u8], length: usize) -> Result<Vec<u8>> {
        let name = String::from_utf8(STANDARD.decode(payload)?)?;
        if !name.starts_with("/meowland-") || name[1..].contains('/') || name.len() > 256 {
            return Err(fail("unexpected shared-memory namespace"));
        }
        let mut file = File::from(shm::open(
            name.as_str(),
            shm::OFlags::RDONLY,
            Mode::empty(),
        )?);
        if file.metadata()?.len() != u64::try_from(length)? {
            return Err(fail("shared image length does not match Kitty dimensions"));
        }
        let mut pixels = vec![0; length];
        let result = file.read_exact(&mut pixels);
        shm::unlink(name.as_str())?;
        result?;
        Ok(pixels)
    }

    pub(crate) fn command(&mut self, body: &[u8]) -> Result<Option<Vec<u8>>> {
        let split = body
            .iter()
            .position(|byte| *byte == b';')
            .ok_or_else(|| fail("Kitty command lacks payload delimiter"))?;
        let control = Control::parse(&body[..split])?;
        let payload = &body[split + 1..];
        match control.get("a") {
            Some("q") => {
                let id = control.number("i", 0)?;
                let supported = if control.get("t") == Some("s") {
                    if self.transport == Transport::Shared {
                        Self::read_shared(payload, 3)?;
                        true
                    } else {
                        false
                    }
                } else {
                    STANDARD.decode(payload)?.len() == 3
                };
                return Ok(Some(
                    format!(
                        "\x1b_Gi={id};{}\x1b\\",
                        if supported { "OK" } else { "ENOTSUP" }
                    )
                    .into_bytes(),
                ));
            }
            Some("d") => {
                match control.get("d") {
                    Some("A") => self.images.clear(),
                    Some("I") => {
                        self.images.remove(&u32::try_from(control.number("i", 0)?)?);
                    }
                    _ => return Err(fail("unsupported Kitty deletion mode")),
                }
                return Ok(None);
            }
            Some("f") if self.transfer.is_some() => {
                let transfer = self.transfer.as_mut().expect("continuation has a transfer");
                if !transfer.edit {
                    return Err(fail("frame continuation interrupts an image transfer"));
                }
                transfer
                    .image
                    .pixels
                    .extend_from_slice(&STANDARD.decode(payload)?);
            }
            Some("T" | "t" | "f") => {
                self.transfer = Some(self.start_transfer(&control, payload)?);
            }
            None if self.transfer.is_some() => {
                let transfer = self.transfer.as_mut().expect("continuation has a transfer");
                transfer
                    .image
                    .pixels
                    .extend_from_slice(&STANDARD.decode(payload)?);
            }
            _ => return Err(fail("unsupported Kitty graphics action")),
        }
        let transfer = self
            .transfer
            .as_ref()
            .expect("graphics action creates a transfer");
        let expected = transfer.image.width * transfer.image.height * 3;
        if transfer.image.pixels.len() > expected * 2 + 65536 {
            return Err(fail("oversized Kitty payload"));
        }
        if control.number("m", 0)? == 0 {
            let (id, quiet) = (transfer.id, transfer.quiet);
            self.finish(expected)?;
            if !quiet {
                return Ok(Some(format!("\x1b_Gi={id};OK\x1b\\").into_bytes()));
            }
        }
        Ok(None)
    }

    fn start_transfer(&self, control: &Control<'_>, payload: &[u8]) -> Result<Transfer> {
        if self.transfer.is_some() {
            return Err(fail("interleaved Kitty transfers"));
        }
        if control.number("f", 0)? != 24 {
            return Err(fail("only RGB Kitty transfers are supported"));
        }
        let width = control.number("s", 0)?;
        let height = control.number("v", 0)?;
        let length = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(3))
            .filter(|length| *length > 0 && *length <= self.size.pixels() * 3)
            .ok_or_else(|| fail("invalid Kitty image dimensions"))?;
        let edit = control.get("a") == Some("f");
        if edit && (control.number("r", 0)? != 1 || control.number("X", 1)? != 1) {
            return Err(fail("only root-frame pixel replacement is supported"));
        }
        let pixels = match control.get("t") {
            Some("s") => Self::read_shared(payload, length)?,
            None | Some("d") => STANDARD.decode(payload)?,
            _ => return Err(fail("unsupported Kitty transport")),
        };
        Ok(Transfer {
            id: u32::try_from(control.number("i", 0)?)?,
            image: Image {
                width,
                height,
                x: if edit {
                    control.number("x", 0)?
                } else {
                    self.cursor.0
                },
                y: if edit {
                    control.number("y", 0)?
                } else {
                    self.cursor.1
                },
                z: control.number("z", 0)?,
                pixels,
            },
            compressed: control.get("o") == Some("z"),
            edit,
            quiet: control.number("q", 0)? != 0,
            transport: if control.get("t") == Some("s") {
                Transport::Shared
            } else {
                Transport::Inline
            },
        })
    }

    fn finish(&mut self, expected: usize) -> Result<()> {
        let mut transfer = self.transfer.take().expect("completed transfer exists");
        if transfer.compressed {
            let mut decoded = Vec::with_capacity(expected);
            ZlibDecoder::new(transfer.image.pixels.as_slice())
                .take(u64::try_from(expected + 1)?)
                .read_to_end(&mut decoded)?;
            transfer.image.pixels = decoded;
        }
        if transfer.image.pixels.len() != expected {
            return Err(fail("incomplete RGB image"));
        }
        self.uploaded = Some(transfer.transport);
        if transfer.edit {
            let destination = self
                .images
                .get_mut(&transfer.id)
                .ok_or_else(|| fail("frame edit has no base image"))?;
            let image = transfer.image;
            if image.x + image.width > destination.width
                || image.y + image.height > destination.height
            {
                return Err(fail("frame edit exceeds base image"));
            }
            for (row, pixels) in image.pixels.chunks_exact(image.width * 3).enumerate() {
                let start = ((image.y + row) * destination.width + image.x) * 3;
                destination.pixels[start..start + pixels.len()].copy_from_slice(pixels);
            }
        } else {
            self.images.insert(transfer.id, transfer.image);
        }
        if self.images.len() > 64 {
            return Err(fail("too many live Kitty images"));
        }
        Ok(())
    }

    pub(crate) fn presented(&mut self) -> Result<Option<u64>> {
        let mut marker = [0; 48];
        for x in 0..16 {
            let image = self
                .images
                .values()
                .filter(|image| {
                    image.y == 0 && image.x <= x && x < image.x + image.width && image.height > 0
                })
                .max_by_key(|image| image.z);
            let Some(image) = image else {
                return Ok(None);
            };
            let at = (x - image.x) * 3;
            marker[x * 3..x * 3 + 3].copy_from_slice(&image.pixels[at..at + 3]);
        }
        let Some(id) = fixture::identify(&marker) else {
            if self.last.is_some() {
                return Err(fail("composited frame marker is corrupt"));
            }
            return Ok(None);
        };
        if self.last == Some(id) {
            return Ok(None);
        }
        self.last = Some(id);
        Ok(Some(id))
    }
}
