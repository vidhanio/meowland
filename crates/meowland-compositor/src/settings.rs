//! Server-owned output and keyboard settings applied when the compositor
//! starts.

use meowland_core::{scale::UiScale, timing::RefreshRate};

/// Output and keyboard policy shared by every pane.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Settings {
    /// Physical pixels per logical pixel.
    pub scale: UiScale,
    /// Advertised output refresh rate and visible frame callback cadence.
    pub refresh_rate: RefreshRate,
    /// XKB keymap and Wayland key repeat policy.
    pub keyboard: KeyboardSettings,
}

/// Keymap names understood by XKB and repeat timings advertised to clients.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct KeyboardSettings {
    /// XKB layout names, separated by commas for multiple layouts.
    pub layout: String,
    /// XKB variants corresponding to the configured layouts.
    pub variant: String,
    /// Comma-separated XKB options, or an empty string for none.
    pub options: String,
    /// Delay before key repeat starts, in milliseconds.
    pub repeat_delay: i32,
    /// Repeated key events per second; zero disables repeat.
    pub repeat_rate: i32,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: 600,
            repeat_rate: 25,
        }
    }
}
