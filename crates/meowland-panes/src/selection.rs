//! Window identity and the compiled selectors panes wait on.

use meowland_protocol::Selector;
use regex::Regex;

/// Compositor-internal identity of a window, never exposed to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    /// Numeric identity in creation order.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for WindowId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// A compiled [`Selector`].
#[derive(Clone, Debug)]
pub enum Matcher {
    /// Match the window with this unique ID.
    Id(String),
    /// Match windows whose application ID matches this expression.
    AppId(Regex),
    /// Match windows whose application ID is exactly this string.
    ExactAppId(String),
    /// Match windows whose title matches this expression.
    Title(Regex),
}

impl Matcher {
    /// Compile a selector received from a client.
    ///
    /// # Errors
    /// Returns an error when a regular expression is invalid.
    pub fn new(selector: Selector) -> Result<Self, regex::Error> {
        Ok(match selector {
            Selector::Id(id) => Self::Id(id),
            Selector::AppId(pattern) => Self::AppId(Regex::new(&pattern)?),
            Selector::Title(pattern) => Self::Title(Regex::new(&pattern)?),
        })
    }

    /// Whether a window with this metadata matches.
    pub fn matches(&self, id: Option<&str>, app_id: &str, title: &str) -> bool {
        match self {
            Self::Id(wanted) => id == Some(wanted.as_str()),
            Self::AppId(pattern) => pattern.is_match(app_id),
            Self::ExactAppId(wanted) => app_id == wanted,
            Self::Title(pattern) => pattern.is_match(title),
        }
    }
}

/// How a pane identifies the windows it may show.
#[derive(Clone, Debug)]
pub enum Selection {
    /// Match windows with this selector.
    Matching(Matcher),
    /// Match the window claimed by this activation token.
    Activation(String),
}
