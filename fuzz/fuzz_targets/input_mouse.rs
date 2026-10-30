#![no_main]

//! The streaming parser's SGR mouse path must agree with the standalone
//! report decoder and always return to ground afterwards.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_input::{
    Event,
    fuzzing::{Parser, mouse},
};

const PREFIX: &[u8] = b"\x1b[<";
const FIELD_BYTES: &[u8] = b"0123456789;-";
/// Longest report the parser buffers before discarding it as oversized.
const BUFFERED: usize = 64;

#[derive(Debug, Arbitrary)]
struct Report {
    raw: Vec<u8>,
    fields: Vec<u8>,
    released: bool,
}

fuzz_target!(|report: Report| {
    let _ = mouse(&report.raw);

    let mut bytes = PREFIX.to_vec();
    bytes.extend(
        report
            .fields
            .iter()
            .map(|&byte| FIELD_BYTES[usize::from(byte) % FIELD_BYTES.len()]),
    );
    bytes.push(if report.released { b'm' } else { b'M' });
    let expected = mouse(&bytes);

    let mut parser = Parser::default();
    for &byte in bytes.iter().chain(b"\x1b[I") {
        parser.push(byte).expect("bounded mouse report");
    }
    let events: Vec<Event> = std::iter::from_fn(|| parser.next_event()).collect();
    let (focus, reports) = events.split_last().expect("focus after mouse report");
    assert!(
        matches!(focus, Event::Focus(true)),
        "parser did not return to ground: {events:?}"
    );
    if bytes.len() <= BUFFERED {
        assert_eq!(
            format!("{reports:?}"),
            format!("{:?}", expected.as_slice()),
            "streaming and standalone mouse decoding disagree"
        );
    } else {
        assert!(reports.is_empty(), "oversized report decoded: {reports:?}");
    }
});
