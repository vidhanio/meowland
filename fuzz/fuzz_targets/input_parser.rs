#![no_main]

//! Terminal input decoding must not depend on how reads split the byte
//! stream, and every decoded event must respect its documented size bound.

mod support;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_input::{Event, fuzzing::Parser};
use meowland_protocol::clipboard::MAX_TEXT;
use support::assert_committed;

#[derive(Debug, Arbitrary)]
struct Stream {
    bytes: Vec<u8>,
    cuts: Vec<u16>,
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

/// Decode `chunks`, draining events after each one as the reader does,
/// including the events a failing chunk queued before its error, so the
/// result does not depend on where the stream was cut.
fn decode<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> (bool, Vec<Event>) {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        let decoded = chunk.iter().all(|&byte| parser.push(byte).is_ok());
        while let Some(event) = parser.next_event() {
            check(&event);
            events.push(event);
        }
        if !decoded {
            return (false, events);
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
