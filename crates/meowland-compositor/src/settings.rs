//! Server-owned output and keyboard settings applied when the compositor
//! starts.

use meowland_core::{pixels::FrameSize, scale::UiScale, timing::RefreshRate};

/// Output and keyboard policy shared by every pane.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Settings {
    /// Fixed virtual monitor resolution, independent of terminal pane size.
    pub output_size: FrameSize,
    /// Physical pixels per logical pixel.
    pub scale: UiScale,
    /// Advertised output refresh rate and visible frame callback cadence.
    pub refresh_rate: RefreshRate,
    /// XKB keymap and Wayland key repeat policy.
    pub keyboard: KeyboardSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_size: FrameSize::new(1920, 1080).expect("default output dimensions are valid"),
            scale: UiScale::default(),
            refresh_rate: RefreshRate::default(),
            keyboard: KeyboardSettings::default(),
        }
    }
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
