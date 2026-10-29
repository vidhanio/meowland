//! Validated compositor UI scale in Wayland's 120ths of a logical pixel.

use std::{fmt, str::FromStr};

use thiserror::Error;

/// A server-wide UI scale shared by every pane, between 0.5 and 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiScale(u32);

/// A scale that is not finite or outside the supported 0.5 to 4 range.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("ui scale must be between 0.5 and 4")]
pub struct InvalidScale;

impl Default for UiScale {
    fn default() -> Self {
        Self(120)
    }
}

impl UiScale {
    /// Physical pixels per logical pixel, quantized to Wayland's 120ths.
    #[must_use]
    pub fn factor(self) -> f64 {
        f64::from(self.0) / 120.0
    }

    /// Round physical pixels to logical pixels, saturating at the positive
    /// `i32` bounds.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "float-to-integer saturation is intentional after rounding and enforcing a positive size"
    )]
    pub fn logical(self, physical: u32) -> i32 {
        (f64::from(physical) / self.factor()).round().max(1.0) as i32
    }
}

impl TryFrom<f64> for UiScale {
    type Error = InvalidScale;

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the validated 0.5..=4.0 scale rounds to an integer in 60..=480"
    )]
    fn try_from(scale: f64) -> Result<Self, Self::Error> {
        if !scale.is_finite() || !(0.5..=4.0).contains(&scale) {
            return Err(InvalidScale);
        }
        Ok(Self((scale * 120.0).round() as u32))
    }
}

impl FromStr for UiScale {
    type Err = InvalidScale;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let scale = value.parse::<f64>().map_err(|_| InvalidScale)?;
        Self::try_from(scale)
    }
}

impl fmt::Display for UiScale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.factor().fmt(formatter)
    }
}
