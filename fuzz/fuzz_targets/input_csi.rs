#![no_main]

//! CSI key decoding must only yield well-formed key, text, or focus events.

mod support;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_input::{Event, fuzzing::csi};
use support::assert_committed;
use vtparse::CsiParam;

#[derive(Debug, Arbitrary)]
enum Param {
    Integer(i64),
    Field,
    Subfield,
    Byte(u8),
}

#[derive(Debug, Arbitrary)]
struct Sequence {
    params: Vec<Param>,
    final_byte: u8,
}

fuzz_target!(|sequence: Sequence| {
    let params: Vec<CsiParam> = sequence
        .params
        .iter()
        .map(|param| match *param {
            Param::Integer(value) => CsiParam::Integer(value),
            Param::Field => CsiParam::P(b';'),
            Param::Subfield => CsiParam::P(b':'),
            Param::Byte(byte) => CsiParam::P(byte),
        })
        .collect();
    match csi(&params, sequence.final_byte) {
        None => {}
        Some(Event::Focus(_)) => assert!(params.is_empty(), "focus with parameters"),
        Some(Event::Key(key)) => {
            assert_ne!(key.code, 0, "key without identity");
            if let Some(text) = &key.text {
                assert_committed(text);
            }
        }
        Some(Event::Text(text)) => assert_committed(&text),
        Some(other) => panic!("unexpected CSI event {other:?}"),
    }
});
