//! meowland: a Wayland compositor that runs inside a terminal.
//!
//! The library holds the whole of it (`meowland::start`). This is only the way
//! in.

fn main() -> anyhow::Result<()> {
    meowland::start()
}
