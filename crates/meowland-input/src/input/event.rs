//! Decoded terminal events.

use meowland_protocol::{Input, Modifiers};

/// Key transition reported by the Kitty keyboard protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Initial press, before any autorepeat events.
    Press,
    /// Autorepeat while the key remains held.
    Repeat,
    /// Release of a previously held key.
    Release,
}

/// Logical key identity, modifier state, and associated text reported by the
/// Kitty protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    /// Unicode code point or Kitty functional-key number.
    pub code: u32,
    /// Modifier bits using [`Modifiers`].
    pub modifiers: Modifiers,
    /// Press, repeat, or release transition.
    pub kind: Kind,
    /// Text the terminal associated with the transition, if reported.
    pub text: Option<String>,
}

/// Terminal focus transition reported by focus-event mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The terminal pane gained focus.
    Gained,
    /// The terminal pane lost focus.
    Lost,
}

/// Decoded terminal event, with bounded text and control-sequence payloads.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Kitty keyboard transition for the compositor to resolve.
    Key(Key),
    /// Committed Unicode text that arrived outside a key event.
    Text(String),
    /// Pixel-accurate pointer input, including signed drag coordinates.
    Pointer(Input),
    /// The terminal pane gained or lost focus.
    Focus(Focus),
    /// Kernel-reported terminal size changed.
    Resize,
    /// Completed bracketed paste containing valid UTF-8.
    Paste(String),
    /// Kitty OSC 5522 metadata and optional payload.
    Clipboard(Vec<u8>),
    /// Base64 payload from an OSC 52 text reply.
    ClipboardText(Vec<u8>),
}
