//! meowland: a Wayland compositor that runs inside a terminal.
//!
//! The library holds the whole of it (`meowland::start`). This is only the way
//! in, and the one place that reports a failure as text.

fn main() -> anyhow::Result<()> {
    meowland::start().map_err(anyhow::Error::from)
}
