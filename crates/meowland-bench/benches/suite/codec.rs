use std::hint::black_box;
use std::time::Instant;

use meowland_core::pixels::FrameSize;
use meowland_kitty::{Presenter, SharedMemory};
use meowland_protocol::{
    FrameBand, Input, PaneAction, PaneToServer, ServerToPane, modifiers, recv, send,
};

fn frame_band(width: u32, height: u32, rgb: Vec<u8>) -> FrameBand {
    FrameBand::new(
        FrameSize::new(width, height).expect("valid benchmark size"),
        0,
        rgb,
    )
    .expect("valid benchmark pixels")
}

use crate::RunLength;
use crate::stats::{Rate, Row, measure};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const CELL: (u16, u16) = (10, 20);
const WARM_UP: usize = 20;

fn pixels(width: u32, height: u32) -> usize {
    FrameSize::new(width, height)
        .expect("benchmark dimensions fit surface limits")
        .rgb_len()
}

fn noise(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut frame = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        frame.push(state.to_le_bytes()[0]);
    }
    frame
}

fn video(seed: u32, width: u32, height: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(pixels(width, height));
    for y in 0..height {
        for x in 0..width {
            let noise = (x.wrapping_mul(17) ^ y.wrapping_mul(31) ^ seed).wrapping_mul(0x9e37_79b9);
            frame.push(((x * 255 / width) ^ (seed & 0xff)).to_le_bytes()[0]);
            frame.push(((y * 255 / height) ^ (seed >> 8)).to_le_bytes()[0]);
            frame.push(noise.to_be_bytes()[0]);
        }
    }
    frame
}

fn put_block(
    frame: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    block_width: u32,
    block_height: u32,
    value: u8,
) {
    for row in 0..block_height {
        let start = usize::try_from((y + row) * width + x)
            .expect("block starts within benchmark frame")
            * 3;
        let bytes = usize::try_from(block_width).expect("block width fits usize") * 3;
        frame[start..start + bytes].fill(value);
    }
}

fn patch_frames(count: usize) -> Vec<Vec<u8>> {
    let base = vec![17; pixels(WIDTH, HEIGHT)];
    (0..count)
        .map(|index| {
            let mut frame = base.clone();
            let x = (u32::try_from(index).expect("patch fixture index fits u32")
                * u32::from(CELL.0)
                * 2)
                % (WIDTH - u32::from(CELL.0));
            put_block(
                &mut frame,
                WIDTH,
                x,
                HEIGHT / 2,
                u32::from(CELL.0),
                u32::from(CELL.1),
                0xab,
            );
            frame
        })
        .collect()
}

fn band_frames(count: usize) -> Vec<Vec<u8>> {
    let base = vec![17; pixels(WIDTH, HEIGHT)];
    (0..count)
        .map(|index| {
            let mut frame = base.clone();
            let y = (u32::try_from(index).expect("band fixture index fits u32")
                * u32::from(CELL.1))
                % (HEIGHT - u32::from(CELL.1));
            put_block(&mut frame, WIDTH, 0, y, WIDTH, u32::from(CELL.1), 0xab);
            frame
        })
        .collect()
}

fn encode_frames(
    label: &'static str,
    mut presenter: Presenter,
    frames: &[Vec<u8>],
    iterations: usize,
) -> Option<Row> {
    let mut out = Vec::with_capacity(64 * 1024);
    let mut next = 0usize;
    let mut frame_iteration = || {
        let frame = frames[next % frames.len()].clone();
        next += 1;
        frame
    };
    for _ in 0..WARM_UP {
        let frame = frame_band(WIDTH, HEIGHT, frame_iteration());
        presenter.present_into(frame, &mut out);
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let frame = frame_band(WIDTH, HEIGHT, frame_iteration());
        let sample = Instant::now();
        presenter.present_into(frame, &mut out);
        samples.push(sample.elapsed());
        black_box(out.len());
    }
    let elapsed = samples.iter().sum();
    Row::new(
        label,
        iterations,
        samples,
        elapsed,
        Rate::bytes(pixels(WIDTH, HEIGHT), iterations),
    )
}

pub(crate) fn terminal(length: RunLength) -> Vec<Row> {
    let iterations = if length == RunLength::Quick { 40 } else { 120 };
    let mut rows = Vec::new();

    let noisy: Vec<Vec<u8>> = (0..4)
        .map(|index| noise(9 + index, pixels(WIDTH, HEIGHT)))
        .collect();
    rows.extend(encode_frames(
        "terminal/encode/full/noise/1080p",
        Presenter::new(None, None),
        &noisy,
        iterations,
    ));

    let video_frames: Vec<Vec<u8>> = (0..4).map(|index| video(index, WIDTH, HEIGHT)).collect();
    rows.extend(encode_frames(
        "terminal/encode/full/video/1080p",
        Presenter::new(None, None),
        &video_frames,
        iterations,
    ));

    rows.extend(encode_frames(
        "terminal/encode/cell/1080p",
        Presenter::new(Some(CELL), None),
        &patch_frames(8),
        iterations,
    ));

    rows.extend(encode_frames(
        "terminal/encode/band/1080p",
        Presenter::new(Some(CELL), None),
        &band_frames(8),
        iterations,
    ));

    let slot = SharedMemory::new();
    if slot.probe().is_some() {
        slot.clear();
        let frames: Vec<_> = (0..4)
            .map(|index| noise(40 + index, pixels(WIDTH, HEIGHT)))
            .collect();
        let mut out = Vec::with_capacity(64 * 1024);
        let mut samples = Vec::with_capacity(iterations);
        for index in 0..iterations {
            let frame = frame_band(WIDTH, HEIGHT, frames[index % frames.len()].clone());
            let mut presenter = Presenter::new(None, Some(SharedMemory::new()));
            let sample = Instant::now();
            presenter.present_into(frame, &mut out);
            samples.push(sample.elapsed());
            black_box(out.len());
            drop(presenter);
        }
        let elapsed = samples.iter().sum();
        rows.extend(Row::new(
            "terminal/encode/shared-memory/noise/1080p",
            iterations,
            samples,
            elapsed,
            Rate::bytes(pixels(WIDTH, HEIGHT), iterations),
        ));
    }
    rows
}

fn frame_message(width: u32, height: u32) -> ServerToPane {
    ServerToPane::Frame(frame_band(width, height, vec![17; pixels(width, height)]))
}

fn wire_frame(prefix: &str, width: u32, height: u32, iterations: usize) -> Vec<Row> {
    let bytes = pixels(width, height);
    let mut rows = Vec::new();
    let message = frame_message(width, height);

    let mut out = Vec::with_capacity(bytes + 64);
    let (samples, elapsed) = measure(WARM_UP, iterations, || {
        out.clear();
        send(&mut out, &message).expect("frame encoding cannot fail");
        black_box(out.len());
    });
    rows.extend(Row::new(
        format!("{prefix}/encode"),
        iterations,
        samples,
        elapsed,
        Rate::bytes(bytes, iterations),
    ));

    let mut encoded = Vec::with_capacity(bytes + 64);
    send(&mut encoded, &message).expect("frame encoding cannot fail");
    let encoded = black_box(encoded);
    let (samples, elapsed) = measure(WARM_UP, iterations, || {
        black_box(
            recv::<ServerToPane>(&mut encoded.as_slice()).expect("decoding sent frame cannot fail"),
        );
    });
    rows.extend(Row::new(
        format!("{prefix}/decode"),
        iterations,
        samples,
        elapsed,
        Rate::bytes(bytes, iterations),
    ));
    rows
}

pub(crate) fn wire(length: RunLength) -> Vec<Row> {
    let quick = length == RunLength::Quick;
    let mut rows = Vec::new();
    rows.extend(wire_frame(
        "wire/frame/1080p",
        1920,
        1080,
        if quick { 120 } else { 400 },
    ));
    rows.extend(wire_frame(
        "wire/frame/2160p",
        3840,
        2160,
        if quick { 40 } else { 160 },
    ));

    let key = PaneToServer::Action(PaneAction::Input(Input::Key {
        code: 30,
        pressed: true,
        modifiers: modifiers::ALT,
        held: vec![56],
    }));
    let mut encoded = Vec::with_capacity(64);
    send(&mut encoded, &key).expect("input encoding cannot fail");
    let encoded = black_box(encoded);
    let iterations = if quick { 800 } else { 4000 };

    let mut out = Vec::with_capacity(64);
    let (samples, elapsed) = measure(WARM_UP, iterations, || {
        out.clear();
        send(&mut out, &key).expect("input encoding cannot fail");
        black_box(out.len());
    });
    rows.extend(Row::new(
        "wire/input/key/encode",
        iterations,
        samples,
        elapsed,
        Rate::Ops,
    ));

    let (samples, elapsed) = measure(WARM_UP, iterations, || {
        black_box(
            recv::<PaneToServer>(&mut encoded.as_slice()).expect("decoding sent input cannot fail"),
        );
    });
    rows.extend(Row::new(
        "wire/input/key/decode",
        iterations,
        samples,
        elapsed,
        Rate::Ops,
    ));
    rows
}
