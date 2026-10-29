//! Server-owned output and keyboard settings applied when the compositor
//! starts.

use meowland_core::{pixels::FrameSize, scale::UiScale, timing::RefreshRate};
use meowland_protocol::{KeyBinding, Modifiers, Shortcuts};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

/// Output and keyboard policy shared by every pane.
///
/// Deserialization validates every field once, so a constructed value always
/// carries a usable output size, scale, refresh rate, and repeat policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "SettingsRepr")]
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
    /// Compositor key bindings shared with panes.
    pub shortcuts: Shortcuts,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_size: FrameSize::new(1920, 1080).expect("default output dimensions are valid"),
            scale: UiScale::default(),
            refresh_rate: RefreshRate::default(),
            keyboard: KeyboardSettings::default(),
            shortcuts: Shortcuts::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsRepr {
    output: OutputRepr,
    keyboard: KeyboardSettings,
    shortcuts: Option<ShortcutsRepr>,
}

/// Shortcut strings; a present table disables bindings it omits.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShortcutsRepr {
    #[serde(deserialize_with = "binding")]
    close_window: Option<KeyBinding>,
    #[serde(deserialize_with = "binding")]
    detach_pane: Option<KeyBinding>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputRepr {
    width: u32,
    height: u32,
    #[serde(deserialize_with = "scale")]
    scale: UiScale,
    #[serde(deserialize_with = "refresh_rate")]
    refresh_rate: RefreshRate,
}

impl Default for OutputRepr {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            width: settings.output_size.width(),
            height: settings.output_size.height(),
            scale: settings.scale,
            refresh_rate: settings.refresh_rate,
        }
    }
}

impl TryFrom<SettingsRepr> for Settings {
    type Error = InvalidOutputSize;

    fn try_from(repr: SettingsRepr) -> Result<Self, Self::Error> {
        let output_size =
            FrameSize::new(repr.output.width, repr.output.height).ok_or(InvalidOutputSize {
                width: repr.output.width,
                height: repr.output.height,
            })?;
        let shortcuts = repr.shortcuts.map_or_default(|shortcuts| Shortcuts {
            close_window: shortcuts.close_window,
            detach_pane: shortcuts.detach_pane,
        });
        Ok(Self {
            output_size,
            scale: repr.output.scale,
            refresh_rate: repr.output.refresh_rate,
            keyboard: repr.keyboard,
            shortcuts,
        })
    }
}

fn scale<'de, D>(deserializer: D) -> Result<UiScale, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    UiScale::try_from(value).map_err(serde::de::Error::custom)
}

fn refresh_rate<'de, D>(deserializer: D) -> Result<RefreshRate, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    RefreshRate::try_from(value).map_err(serde::de::Error::custom)
}

/// Output dimensions that are empty or exceed the supported frame limits.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("output dimensions {width}x{height} are outside the supported limits")]
#[non_exhaustive]
pub struct InvalidOutputSize {
    /// Configured output width in physical pixels.
    pub width: u32,
    /// Configured output height in physical pixels.
    pub height: u32,
}

/// Keymap names understood by XKB and repeat timings advertised to clients.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct KeyboardSettings {
    /// XKB layout names, separated by commas for multiple layouts.
    pub layout: String,
    /// XKB variants corresponding to the configured layouts.
    pub variant: String,
    /// Comma-separated XKB options, or an empty string for none.
    pub options: String,
    /// Delay before key repeat starts, in milliseconds.
    pub repeat_delay: RepeatDelay,
    /// Repeated key events per second; zero disables repeat.
    pub repeat_rate: RepeatRate,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: RepeatDelay::default(),
            repeat_rate: RepeatRate::default(),
        }
    }
}

/// A validated nonnegative key repeat delay in milliseconds.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "i32")]
pub struct RepeatDelay(i32);

impl Default for RepeatDelay {
    fn default() -> Self {
        Self(600)
    }
}

impl RepeatDelay {
    /// Delay in milliseconds understood by the seat keyboard.
    #[must_use]
    pub const fn milliseconds(self) -> i32 {
        self.0
    }
}

impl TryFrom<i32> for RepeatDelay {
    type Error = InvalidRepeatDelay;

    fn try_from(milliseconds: i32) -> Result<Self, Self::Error> {
        (milliseconds >= 0)
            .then_some(Self(milliseconds))
            .ok_or(InvalidRepeatDelay)
    }
}

/// A negative key repeat delay.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("key repeat delay must be nonnegative")]
pub struct InvalidRepeatDelay;

/// A validated nonnegative key repeat rate in events per second.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "i32")]
pub struct RepeatRate(i32);

impl Default for RepeatRate {
    fn default() -> Self {
        Self(25)
    }
}

impl RepeatRate {
    /// Rate in events per second understood by the seat keyboard; zero
    /// disables repeat.
    #[must_use]
    pub const fn hertz(self) -> i32 {
        self.0
    }
}

impl TryFrom<i32> for RepeatRate {
    type Error = InvalidRepeatRate;

    fn try_from(events_per_second: i32) -> Result<Self, Self::Error> {
        (events_per_second >= 0)
            .then_some(Self(events_per_second))
            .ok_or(InvalidRepeatRate)
    }
}

/// A negative key repeat rate.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("key repeat rate must be nonnegative")]
pub struct InvalidRepeatRate;

fn binding<'de, D>(deserializer: D) -> Result<Option<KeyBinding>, D::Error>
where
    D: Deserializer<'de>,
{
    parse_binding(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

fn parse_binding(value: &str) -> Result<Option<KeyBinding>, InvalidShortcut> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let mut parts = value.split('+').map(str::trim);
    let key = parts
        .next_back()
        .ok_or_else(|| InvalidShortcut::new(value))?;
    let mut modifiers = Modifiers::empty();
    for part in parts {
        let modifier = match part.to_ascii_lowercase().as_str() {
            "shift" => Modifiers::SHIFT,
            "ctrl" | "control" => Modifiers::CONTROL,
            "alt" => Modifiers::ALT,
            "super" => Modifiers::SUPER,
            "hyper" => Modifiers::HYPER,
            "meta" => Modifiers::META,
            _ => return Err(InvalidShortcut::new(value)),
        };
        modifiers |= modifier;
    }
    let mut characters = key.chars();
    let Some(character) = characters.next() else {
        return Err(InvalidShortcut::new(value));
    };
    if characters.next().is_some() || character.is_control() {
        return Err(InvalidShortcut::new(value));
    }
    let key = if character.is_ascii() {
        u32::from(character.to_ascii_lowercase())
    } else {
        u32::from(character)
    };
    Ok(Some(KeyBinding { key, modifiers }))
}

#[derive(Debug, Error)]
#[error("invalid shortcut `{value}`; use modifiers joined to one key, such as `ctrl+shift+q`")]
struct InvalidShortcut {
    value: String,
}

impl InvalidShortcut {
    fn new(value: &str) -> Self {
        Self {
            value: value.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_binding_accepts_modifiers_and_one_key() {
        let cases = [
            (
                "q",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::empty(),
                },
            ),
            (
                "alt+q",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::ALT,
                },
            ),
            (
                "control+shift+Q",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::CONTROL | Modifiers::SHIFT,
                },
            ),
            (
                " ctrl + alt + q ",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::CONTROL | Modifiers::ALT,
                },
            ),
            (
                "SUPER+Hyper+meta+alt+q",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::SUPER
                        | Modifiers::HYPER
                        | Modifiers::META
                        | Modifiers::ALT,
                },
            ),
            (
                "alt+é",
                KeyBinding {
                    key: u32::from('é'),
                    modifiers: Modifiers::ALT,
                },
            ),
            (
                "alt+alt+q",
                KeyBinding {
                    key: u32::from(b'q'),
                    modifiers: Modifiers::ALT,
                },
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                parse_binding(value).expect("valid shortcut"),
                Some(expected),
                "{value}"
            );
        }
    }

    #[test]
    fn parse_binding_treats_blank_values_as_disabled() {
        assert_eq!(parse_binding("").expect("blank shortcut"), None);
        assert_eq!(parse_binding("   ").expect("blank shortcut"), None);
    }

    #[test]
    fn parse_binding_rejects_invalid_shortcuts() {
        for value in [
            "alt",
            "alt+",
            "alt++q",
            "alt+qq",
            "alt+q+r",
            "foo+q",
            "alt+\u{7f}",
            "alt+\n",
            "+",
        ] {
            assert!(parse_binding(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn parse_binding_reports_the_invalid_value() {
        let error = parse_binding(" alt+qq ").expect_err("invalid shortcut");
        assert_eq!(
            error.to_string(),
            "invalid shortcut `alt+qq`; use modifiers joined to one key, such as `ctrl+shift+q`"
        );
    }
}
