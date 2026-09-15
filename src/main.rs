//! meowland: a Wayland compositor that runs inside your terminal.
//!
//! The library is the whole of it (`meowland::start`); this is only the way in.

fn main() -> anyhow::Result<()> {
    meowland::start()
}
