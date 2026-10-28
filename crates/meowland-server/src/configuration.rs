//! Server-owned configuration schema, independent of file discovery and
//! loading.

use std::io;

use meowland_compositor::Settings;
use meowland_core::pixels::FrameSize;
use serde::Deserialize;

/// Server-wide output and input policy, applied once at startup.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct Config {
    /// Virtual output scale and refresh rate.
    pub output: OutputConfig,
    /// XKB keymap and Wayland key repeat policy.
    pub keyboard: KeyboardConfig,
}

/// Virtual output settings shared by every pane.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputConfig {
    /// Fixed virtual monitor width in physical pixels.
    pub width: u32,
    /// Fixed virtual monitor height in physical pixels.
    pub height: u32,
    /// Physical pixels per logical pixel, between 0.5 and 4.
    pub scale: f64,
    /// Advertised refresh rate and visible callback cadence, between 1 and 1000
    /// Hz.
    pub refresh_rate: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 120,
        }
    }
}

/// XKB keymap names and repeat timings advertised to Wayland clients.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct KeyboardConfig {
    /// XKB layout names, separated by commas for multiple layouts.
    pub layout: String,
    /// XKB variants corresponding to the configured layouts.
    pub variant: String,
    /// Comma-separated XKB options, or an empty string for none.
    pub options: String,
    /// Delay before key repeat starts, in milliseconds; must be nonnegative.
    pub repeat_delay: i32,
    /// Repeated key events per second; zero disables repeat and negatives are
    /// invalid.
    pub repeat_rate: i32,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        let keyboard = Settings::default().keyboard;
        Self {
            layout: keyboard.layout,
            variant: keyboard.variant,
            options: keyboard.options,
            repeat_delay: keyboard.repeat_delay,
            repeat_rate: keyboard.repeat_rate,
        }
    }
}

impl Config {
    pub(crate) fn into_settings(self) -> io::Result<Settings> {
        if self.keyboard.repeat_delay < 0 || self.keyboard.repeat_rate < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "keyboard repeat delay and rate must be nonnegative",
            ));
        }
        let mut settings = Settings::default();
        settings.output_size =
            FrameSize::new(self.output.width, self.output.height).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid output dimensions")
            })?;
        settings.scale = self.output.scale.to_string().parse()?;
        settings.refresh_rate = self.output.refresh_rate.try_into()?;
        settings.keyboard.layout = self.keyboard.layout;
        settings.keyboard.variant = self.keyboard.variant;
        settings.keyboard.options = self.keyboard.options;
        settings.keyboard.repeat_delay = self.keyboard.repeat_delay;
        settings.keyboard.repeat_rate = self.keyboard.repeat_rate;
        Ok(settings)
    }
}
