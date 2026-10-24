//! Validated compositor UI scale in Wayland's 120ths of a logical pixel.

use std::{fmt, io, str::FromStr};

/// A server-wide UI scale shared by every pane, between 0.5 and 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiScale(u32);

impl Default for UiScale {
    fn default() -> Self {
        Self(120)
    }
}

impl UiScale {
    #[must_use]
    pub fn factor(self) -> f64 {
        f64::from(self.0) / 120.0
    }

    #[must_use]
    pub fn logical(self, physical: u32) -> i32 {
        (f64::from(physical) / self.factor()).round().max(1.0) as i32
    }
}

impl FromStr for UiScale {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let scale = value
            .parse::<f64>()
            .ok()
            .filter(|scale| scale.is_finite() && (0.5..=4.0).contains(scale));
        scale.map_or_else(
            || {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "UI scale must be between 0.5 and 4",
                ))
            },
            |scale| Ok(Self((scale * 120.0).round() as u32)),
        )
    }
}

impl fmt::Display for UiScale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.factor().fmt(formatter)
    }
}
