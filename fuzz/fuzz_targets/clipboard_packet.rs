#![no_main]

//! Arbitrary OSC 5522 and OSC 52 replies interleaved with clipboard state
//! changes must never panic the terminal clipboard.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_protocol::clipboard::Selection;
use meowland_terminal_clipboard::{Clipboard, Focus, Protocol, Refresh};

#[derive(Debug, Arbitrary)]
enum Op {
    Packet(Vec<u8>),
    Text(Vec<u8>),
    Focus(bool),
    Refresh { primary: bool, paste: bool },
    Advance,
}

#[derive(Debug, Arbitrary)]
struct Session {
    kitty: bool,
    ops: Vec<Op>,
}

fuzz_target!(|session: Session| {
    let protocol = if session.kitty {
        Protocol::Kitty
    } else {
        Protocol::Osc52
    };
    let mut clipboard = Clipboard::new(protocol).expect("clipboard credentials");
    let mut output = Vec::new();
    for op in session.ops {
        match op {
            Op::Packet(bytes) => clipboard.packet(&bytes),
            Op::Text(bytes) => clipboard.text(&bytes),
            Op::Focus(gained) => {
                clipboard.focus(if gained { Focus::Gained } else { Focus::Lost });
            }
            Op::Refresh { primary, paste } => {
                let selection = if primary {
                    Selection::Primary
                } else {
                    Selection::Clipboard
                };
                let refresh = if paste {
                    Refresh::Paste
                } else {
                    Refresh::Background
                };
                clipboard.refresh(selection, refresh);
            }
            Op::Advance => {
                output.clear();
                clipboard.advance(&mut output);
            }
        }
        while clipboard.next_action().is_some() {}
    }
});
