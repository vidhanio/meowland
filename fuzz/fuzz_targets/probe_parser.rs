#![no_main]

//! Capability replies must decode identically however terminal reads split
//! them.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meowland_terminal::fuzzing::ProbeParser;

#[derive(Debug, Arbitrary)]
struct Replies {
    bytes: Vec<u8>,
    cuts: Vec<u16>,
}

fuzz_target!(|replies: Replies| {
    let bytes = replies.bytes.as_slice();
    let mut cuts: Vec<usize> = replies
        .cuts
        .iter()
        .map(|&cut| usize::from(cut) % (bytes.len() + 1))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut whole = ProbeParser::default();
    whole.parse(bytes);

    let mut split = ProbeParser::default();
    let mut start = 0;
    for end in cuts.into_iter().chain(std::iter::once(bytes.len())) {
        split.parse(&bytes[start..end]);
        start = end;
    }

    assert_eq!(whole.attributes_seen(), split.attributes_seen());
    assert_eq!(whole.finish(), split.finish());
});
