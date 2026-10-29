//! Bounded, MIME-aware clipboard data and shared monotonic request deadlines.

use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 text bytes accepted by bracketed paste and legacy OSC 52.
pub const MAX_TEXT: usize = 1024 * 1024;
/// Maximum aggregate bytes across all MIME representations of a selection.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
/// Maximum distinct MIME representations advertised for one selection.
pub const MAX_TYPES: usize = 64;
/// Canonical UTF-8 text representations offered for plain-text selections.
pub const TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

const MAX_MIME_LENGTH: usize = 256;

/// The selection a clipboard transfer belongs to.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Selection {
    /// Explicit copy-and-paste selection.
    #[default]
    Clipboard,
    /// Selection commonly pasted with the middle mouse button.
    Primary,
}

/// Terminal clipboard transport, deciding which representations an export
/// needs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Transport {
    /// MIME-aware, permission-aware OSC 5522 transfers.
    Kitty,
    /// Legacy OSC 52 plain-text transfers.
    Osc52,
}

/// One MIME representation of clipboard contents, validated by the receiving
/// transport.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
    /// MIME type or opaque Wayland selection target, such as `UTF8_STRING`.
    pub mime: String,
    /// Raw representation bytes, not necessarily UTF-8.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Selection contents in multiple formats; call [`Self::valid`] before
/// accepting external data.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClipboardData {
    /// Available representations in preference order; an empty list clears the
    /// selection.
    pub items: Vec<Item>,
}

impl ClipboardData {
    /// Offer text under both canonical UTF-8 MIME types without truncation.
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

    /// Check aggregate size, representation count, and MIME syntax against
    /// transport limits.
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

/// Accept a bounded printable MIME type or opaque Wayland target, excluding the
/// list sentinel.
#[must_use]
pub fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime != "."
        && mime.len() <= MAX_MIME_LENGTH
        && mime.bytes().all(|byte| byte.is_ascii_graphic())
        && (!mime.contains('/')
            || mime
                .parse::<mime::Mime>()
                .is_ok_and(|media| !media.subtype().as_str().is_empty()))
}

/// Filter invalid or duplicate types, preserving order and retaining at most
/// [`MAX_TYPES`].
#[must_use]
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
    /// Start a timeout on the boot-scoped monotonic clock, saturating on
    /// overflow.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        Self(Self::now().saturating_add(nanos))
    }

    /// Time until expiry, or zero once the deadline has passed.
    #[must_use]
    pub fn remaining(self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(Self::now()))
    }

    /// Whether the monotonic clock has reached this deadline.
    #[must_use]
    pub fn expired(self) -> bool {
        self.remaining().is_zero()
    }

    fn now() -> u64 {
        let time = clock_gettime(ClockId::Monotonic);
        u64::try_from(time.tv_sec)
            .unwrap_or_default()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_nsec).unwrap_or_default())
    }
}

/// Transfer timeout, including time spent awaiting terminal clipboard
/// permission.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
