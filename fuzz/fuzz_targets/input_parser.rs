#![no_main]

//! Terminal input decoding must not depend on how reads split the byte
//! stream, and every decoded event must respect its documented size bound.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_input::{Event, fuzzing::Parser};
use meowland_protocol::{MAX_INPUT_TEXT, clipboard::MAX_TEXT};

#[derive(Debug, Arbitrary)]
struct Stream {
    bytes: Vec<u8>,
    cuts: Vec<u16>,
}

fn assert_committed(text: &str) {
    assert!(!text.is_empty(), "empty committed text");
    assert!(text.len() <= MAX_INPUT_TEXT, "oversized committed text");
    assert!(
        !text.chars().any(char::is_control),
        "control character in committed text: {text:?}"
    );
}

fn check(event: &Event) {
    match event {
        Event::Text(text) => assert_committed(text),
        Event::Key(key) => {
            if let Some(text) = &key.text {
                assert_committed(text);
            }
        }
        Event::Paste(text) => assert!(text.len() <= MAX_TEXT, "oversized paste"),
        _ => {}
    }
}

/// Decode `chunks`, draining events after each one as the reader does.
fn decode<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> (bool, Vec<String>) {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        for &byte in chunk {
            if parser.push(byte).is_err() {
                return (false, events);
            }
        }
        while let Some(event) = parser.next_event() {
            check(&event);
            events.push(format!("{event:?}"));
        }
    }
    (true, events)
}

fuzz_target!(|stream: Stream| {
    let bytes = stream.bytes.as_slice();
    let mut cuts: Vec<usize> = stream
        .cuts
        .iter()
        .map(|&cut| usize::from(cut) % (bytes.len() + 1))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let chunks = std::iter::once(0)
        .chain(cuts.iter().copied())
        .zip(cuts.iter().copied().chain(std::iter::once(bytes.len())))
        .map(|(start, end)| &bytes[start..end]);
    assert_eq!(decode([bytes]), decode(chunks));
});
