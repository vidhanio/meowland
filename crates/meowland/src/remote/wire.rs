//! Versioned binary transport with bounded, temporally compressed RGB bands.

use std::{
    io::{self, Write},
    time::Duration,
};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use meowland_core::pixels::FrameSize;
use meowland_protocol::{
    ServerToPane,
    clipboard::{Deadline, TRANSFER_TIMEOUT},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const GREETING: &[u8] = b"MEOWLAND-SSH\0\x01";

#[derive(Deserialize, Serialize)]
pub(super) enum Message {
    Event(ServerToPane),
    Frame {
        width: u32,
        height: u32,
        y: u32,
        bytes: u32,
        encoding: Encoding,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    ClipboardRead {
        request: u64,
        offer: u64,
        mime: String,
        remaining: Duration,
    },
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum Encoding {
    Raw,
    Zlib,
    Delta,
}

#[derive(Default)]
pub(super) struct Frames {
    size: Option<FrameSize>,
    previous: Vec<u8>,
    delta: Vec<u8>,
}

impl Frames {
    pub(super) fn encode(&mut self, message: ServerToPane) -> io::Result<Message> {
        match message {
            ServerToPane::Frame {
                width,
                height,
                y,
                rgb,
            } => {
                let size = frame_size(width, height)?;
                let range = size
                    .rgb_band(y, rgb.len())
                    .ok_or_else(|| invalid("invalid RGB band"))?;
                let delta = self.size == Some(size);
                self.resize(size);
                let input = if delta {
                    self.delta.clear();
                    self.delta.extend(
                        rgb.iter()
                            .zip(&self.previous[range.clone()])
                            .map(|(new, old)| new ^ old),
                    );
                    &self.delta
                } else {
                    &rgb
                };
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
                encoder.write_all(input)?;
                let compressed = encoder.finish()?;
                self.previous[range].copy_from_slice(&rgb);
                let bytes = rgb.len() as u32;
                let (encoding, data) = if compressed.len() < rgb.len() {
                    (
                        if delta {
                            Encoding::Delta
                        } else {
                            Encoding::Zlib
                        },
                        compressed,
                    )
                } else {
                    (Encoding::Raw, rgb)
                };
                Ok(Message::Frame {
                    width,
                    height,
                    y,
                    bytes,
                    encoding,
                    data,
                })
            }
            ServerToPane::ClipboardRead {
                request,
                offer,
                mime,
                deadline,
            } => Ok(Message::ClipboardRead {
                request,
                offer,
                mime,
                remaining: deadline.remaining(),
            }),
            event => Ok(Message::Event(event)),
        }
    }

    pub(super) fn decode(&mut self, message: Message) -> io::Result<ServerToPane> {
        match message {
            Message::Frame {
                width,
                height,
                y,
                bytes,
                encoding,
                data,
            } => {
                let size = frame_size(width, height)?;
                let range = size
                    .rgb_band(y, bytes as usize)
                    .ok_or_else(|| invalid("invalid RGB band"))?;
                if matches!(encoding, Encoding::Delta) && self.size != Some(size) {
                    return Err(invalid("delta frame has no matching base"));
                }
                let mut rgb = if matches!(encoding, Encoding::Raw) {
                    data
                } else {
                    let mut output = Vec::with_capacity(bytes as usize + 1);
                    let mut decoder = Decompress::new(true);
                    let status = decoder
                        .decompress_vec(&data, &mut output, FlushDecompress::Finish)
                        .map_err(invalid)?;
                    if status != Status::StreamEnd || decoder.total_in() != data.len() as u64 {
                        return Err(invalid("invalid compressed frame"));
                    }
                    output
                };
                if rgb.len() != bytes as usize {
                    return Err(invalid("incorrect RGB band length"));
                }
                self.resize(size);
                if matches!(encoding, Encoding::Delta) {
                    for (new, old) in rgb.iter_mut().zip(&self.previous[range.clone()]) {
                        *new ^= old;
                    }
                }
                self.previous[range].copy_from_slice(&rgb);
                Ok(ServerToPane::Frame {
                    width,
                    height,
                    y,
                    rgb,
                })
            }
            Message::ClipboardRead {
                request,
                offer,
                mime,
                remaining,
            } => Ok(ServerToPane::ClipboardRead {
                request,
                offer,
                mime,
                deadline: Deadline::new(remaining.min(TRANSFER_TIMEOUT)),
            }),
            Message::Event(ServerToPane::Frame { .. } | ServerToPane::ClipboardRead { .. }) => {
                Err(invalid("event requires remote framing"))
            }
            Message::Event(event) => Ok(event),
        }
    }

    fn resize(&mut self, size: FrameSize) {
        if self.size != Some(size) {
            self.previous.clear();
            self.previous.resize(size.rgb_len(), 0);
            self.size = Some(size);
        }
    }
}

pub(super) async fn greet(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    writer.write_all(GREETING).await?;
    writer.flush().await?;
    let mut greeting = [0; GREETING.len()];
    reader.read_exact(&mut greeting).await?;
    if greeting != GREETING {
        return Err(invalid(
            "incompatible remote protocol; update meowland on both hosts",
        ));
    }
    Ok(())
}

fn frame_size(width: u32, height: u32) -> io::Result<FrameSize> {
    FrameSize::new(width, height).ok_or_else(|| invalid("invalid frame dimensions"))
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
