//! Bounded, MIME-aware clipboard data and shared monotonic request deadlines.

use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};

pub const MAX_TEXT: usize = 1024 * 1024;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TYPES: usize = 64;
pub const TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

const MAX_MIME_LENGTH: usize = 256;

/// The selection a clipboard transfer belongs to.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Selection {
    #[default]
    Clipboard,
    Primary,
}

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
    #[must_use]
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

    #[must_use]
    pub fn valid(&self) -> bool {
        self.items.len() <= MAX_TYPES
            && self
                .items
                .iter()
                .try_fold(0usize, |total, item| total.checked_add(item.data.len()))
                .is_some_and(|total| total <= MAX_BYTES)
            && self.items.iter().all(|item| valid_mime(&item.mime))
    }

    /// UTF-8 plain text for legacy OSC 52, including Wayland's opaque alias.
    #[must_use]
    pub fn utf8_text(&self) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|item| {
                is_utf8_text_mime(&item.mime)
                    && item.data.len() <= MAX_TEXT
                    && std::str::from_utf8(&item.data).is_ok()
            })
            .map(|item| item.data.as_slice())
    }
}

fn is_utf8_text_mime(mime: &str) -> bool {
    mime == "UTF8_STRING"
        || mime.parse::<mime::Mime>().is_ok_and(|media| {
            media.type_() == mime::TEXT
                && media.subtype() == mime::PLAIN
                && media
                    .get_param(mime::CHARSET)
                    .is_none_or(|charset| charset == mime::UTF_8)
        })
}

#[must_use]
pub fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime != "."
        && mime.len() <= MAX_MIME_LENGTH
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

/// A boot-scoped `CLOCK_MONOTONIC` deadline shared by local server and pane
/// processes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Deadline(u64);

impl Deadline {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self(Self::now().saturating_add(timeout.as_nanos().min(u128::from(u64::MAX)) as u64))
    }

    #[must_use]
    pub fn remaining(self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(Self::now()))
    }

    #[must_use]
    pub fn expired(self) -> bool {
        self.remaining().is_zero()
    }

    fn now() -> u64 {
        let time = clock_gettime(ClockId::Monotonic);
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64)
    }
}

pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
