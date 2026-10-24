use std::{hint::black_box, time::Instant};

use meowland_kitty::{Presenter, SharedMemory};
use meowland_protocol::{Input, PaneToServer, ServerToPane, modifiers, recv, send};

use crate::stats::{Rate, Row, measure};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const CELL: (u16, u16) = (10, 20);
const WARM_UP: usize = 20;

const fn pixels(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3
}

fn noise(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut frame = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        frame.push(state as u8);
    }
    frame
}

fn video(seed: u32, width: u32, height: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(pixels(width, height));
    for y in 0..height {
        for x in 0..width {
            let noise = (x.wrapping_mul(17) ^ y.wrapping_mul(31) ^ seed).wrapping_mul(0x9e37_79b9);
            frame.push(((x * 255 / width) ^ (seed & 0xff)) as u8);
            frame.push(((y * 255 / height) ^ (seed >> 8)) as u8);
            frame.push((noise >> 24) as u8);
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
        let start = ((y + row) * width + x) as usize * 3;
        frame[start..start + (block_width * 3) as usize].fill(value);
    }
}

fn patch_frames(count: usize) -> Vec<Vec<u8>> {
    let base = vec![17; pixels(WIDTH, HEIGHT)];
    (0..count)
        .map(|index| {
            let mut frame = base.clone();
            let x = (index as u32 * u32::from(CELL.0) * 2) % (WIDTH - u32::from(CELL.0));
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
            let y = (index as u32 * u32::from(CELL.1)) % (HEIGHT - u32::from(CELL.1));
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
        let frame = frame_iteration();
        presenter.present_into(WIDTH, HEIGHT, 0, frame, &mut out);
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let frame = frame_iteration();
        let sample = Instant::now();
        presenter.present_into(WIDTH, HEIGHT, 0, frame, &mut out);
        samples.push(sample.elapsed());
        black_box(out.len());
    }
    let elapsed = samples.iter().sum();
    Row::new(
        label,
        iterations,
        samples,
        elapsed,
        Rate::Bytes(pixels(WIDTH, HEIGHT) as u64 * iterations as u64),
    )
}

pub fn terminal(quick: bool) -> Vec<Row> {
    let iterations = if quick { 40 } else { 120 };
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
            let frame = frames[index % frames.len()].clone();
            let mut presenter = Presenter::new(None, Some(SharedMemory::new()));
            let sample = Instant::now();
            presenter.present_into(WIDTH, HEIGHT, 0, frame, &mut out);
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
            Rate::Bytes(pixels(WIDTH, HEIGHT) as u64 * iterations as u64),
        ));
    }
    rows
}

fn frame_message(width: u32, height: u32) -> ServerToPane {
    ServerToPane::Frame {
        width,
        height,
        y: 0,
        rgb: vec![17; pixels(width, height)],
    }
}

fn wire_frame(prefix: &str, width: u32, height: u32, iterations: usize) -> Vec<Row> {
    let bytes = pixels(width, height) as u64;
    let mut rows = Vec::new();
    let message = frame_message(width, height);

    let mut out = Vec::with_capacity(bytes as usize + 64);
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
        Rate::Bytes(bytes * iterations as u64),
    ));

    let mut encoded = Vec::with_capacity(bytes as usize + 64);
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
        Rate::Bytes(bytes * iterations as u64),
    ));
    rows
}

pub fn wire(quick: bool) -> Vec<Row> {
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

    let key = PaneToServer::Input(Input::Key {
        code: 30,
        pressed: true,
        modifiers: modifiers::ALT,
        held: vec![56],
    });
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
