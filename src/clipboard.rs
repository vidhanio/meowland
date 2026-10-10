//! Bounded, MIME-aware clipboard data and nonblocking Wayland transfers.

use std::{
    io,
    os::fd::OwnedFd,
    time::{Duration, Instant},
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use serde::{Deserialize, Serialize};

pub const MAX_TEXT: usize = 1024 * 1024;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TYPES: usize = 64;
pub const TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
    pub mime: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClipboardData {
    pub items: Vec<Item>,
}

impl ClipboardData {
    pub fn text(text: &str) -> Self {
        Self {
            items: TEXT_MIMES
                .into_iter()
                .map(|mime| Item {
                    mime: mime.to_owned(),
                    data: text.as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    pub fn valid(&self) -> bool {
        self.items.len() <= MAX_TYPES
            && self.items.iter().all(|item| valid_mime(&item.mime))
            && self.items.iter().map(|item| item.data.len()).sum::<usize>() <= MAX_BYTES
    }

    /// UTF-8 plain text for legacy OSC 52, including Wayland's opaque alias.
    pub fn utf8_text(&self) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|item| {
                let text = item.mime == "UTF8_STRING"
                    || item.mime.parse::<mime::Mime>().is_ok_and(|media| {
                        media.type_() == mime::TEXT
                            && media.subtype() == mime::PLAIN
                            && media
                                .get_param(mime::CHARSET)
                                .is_none_or(|charset| charset == mime::UTF_8)
                    });
                text && item.data.len() <= MAX_TEXT && std::str::from_utf8(&item.data).is_ok()
            })
            .map(|item| item.data.as_slice())
    }
}

pub fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime != "."
        && mime.len() <= 256
        && mime.bytes().all(|byte| byte.is_ascii_graphic())
        && (!mime.contains('/') || mime.parse::<mime::Mime>().is_ok())
}

pub fn mime_types(types: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut result = Vec::new();
    for mime in types {
        if result.len() == MAX_TYPES {
            break;
        }
        if valid_mime(&mime) && !result.contains(&mime) {
            result.push(mime);
        }
    }
    result
}

#[derive(Debug)]
pub struct Transfer {
    fd: OwnedFd,
    bytes: Vec<u8>,
    deadline: Instant,
}

impl Transfer {
    pub(crate) fn new(fd: OwnedFd) -> io::Result<Self> {
        fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
        Ok(Self {
            fd,
            bytes: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(75),
        })
    }

    pub(crate) fn receive(&mut self) -> io::Result<Option<Vec<u8>>> {
        if Instant::now() >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut bytes = [0; 16 * 1024];
        for _ in 0..4 {
            match rustix::io::read(&self.fd, &mut bytes) {
                Ok(0) => return Ok(Some(std::mem::take(&mut self.bytes))),
                Ok(read) => {
                    if self.bytes.len() + read > MAX_BYTES {
                        return Err(io::Error::other("clipboard transfer exceeds size limit"));
                    }
                    self.bytes.extend_from_slice(&bytes[..read]);
                }
                Err(Errno::AGAIN | Errno::INTR) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }
}
