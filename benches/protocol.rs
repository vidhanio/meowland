//! Benchmarks for the wire codec the sockets speak.
//!
//! A frame crosses the pane socket as one `bincode` message, so encoding and
//! decoding a whole screen are both per-frame costs.  Input messages go the
//! other way and are the per-event cost of a keystroke, a paste or a mouse
//! move.  `send` grows the buffer it writes into and `recv` allocates the
//! message it returns, which is what their callers do too, so both are paid
//! for here.
//!
//! `cargo test --all-targets` runs each case once, in criterion's test mode.

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use meowland::protocol::{
    ControlResponse, Input, PaneToServer, ServerToPane, WindowInfo, modifiers, recv, send,
};

/// A laptop screen and a 4K one: the message is pixels, so it scales with them.
const SIZES: [(u32, u32, &str); 2] = [(1920, 1080, "1080p"), (3840, 2160, "2160p")];
/// A busy desktop's worth of windows for `meowland list` to answer with.
const WINDOWS: u64 = 64;
/// A codec is a memcpy with a format around it, so a few thousand iterations
/// per case say more than a longer wall clock does; the measurement still runs
/// for seconds so a 4K frame gets its samples.
const SAMPLES: usize = 30;
const MEASUREMENT: Duration = Duration::from_secs(3);
const WARM_UP: Duration = Duration::from_secs(1);

const fn pixels(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3
}

fn frame(width: u32, height: u32) -> ServerToPane {
    ServerToPane::Frame {
        width,
        height,
        y: 0,
        rgb: vec![17; pixels(width, height)],
    }
}

fn frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire/frame");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    for (width, height, name) in SIZES {
        let bytes = pixels(width, height);
        group.throughput(Throughput::Bytes(bytes as u64));

        // Server to pane: the pixels are copied into the message, and the
        // message into the socket.
        group.bench_function(BenchmarkId::new("encode", name), |b| {
            b.iter_batched(
                || (frame(width, height), Vec::with_capacity(bytes + 64)),
                |(message, mut out)| {
                    send(&mut out, &message).expect("encoding a frame cannot fail");
                    black_box(out)
                },
                BatchSize::LargeInput,
            );
        });

        // Pane to server: the frame arrives as bytes and lands in one
        // allocation.
        group.bench_function(BenchmarkId::new("decode", name), |b| {
            b.iter_batched(
                || {
                    let mut out = Vec::with_capacity(bytes + 64);
                    send(&mut out, &frame(width, height)).expect("encoding a frame cannot fail");
                    out
                },
                |encoded| {
                    black_box(
                        recv::<ServerToPane>(&mut encoded.as_slice())
                            .expect("decoding what send produced cannot fail"),
                    )
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

fn input(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire/input");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    let key = PaneToServer::Input(Input::Key {
        code: 30,
        pressed: true,
        modifiers: modifiers::ALT,
    });
    let pointer = PaneToServer::Input(Input::Pointer {
        x: 640.0,
        y: 360.0,
        button: None,
        pressed: false,
        scroll: 0,
    });

    for (name, message) in [("key", &key), ("pointer", &pointer)] {
        // A pane writes input into a buffer it keeps, so the buffer lives
        // outside the measured loop: this is the per-event cost alone.
        let mut out = Vec::with_capacity(64);
        group.bench_with_input(BenchmarkId::new("encode", name), message, |b, message| {
            b.iter(|| {
                out.clear();
                send(&mut out, message).expect("encoding an input cannot fail");
            });
        });

        let mut encoded = Vec::with_capacity(64);
        send(&mut encoded, message).expect("encoding an input cannot fail");
        let encoded = black_box(encoded);
        group.bench_function(BenchmarkId::new("decode", name), |b| {
            b.iter(|| {
                black_box(
                    recv::<PaneToServer>(&mut encoded.as_slice())
                        .expect("decoding what send produced cannot fail"),
                )
            });
        });
    }

    group.finish();
}

fn control(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire/control");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    // `meowland list` answers with every window the server has.
    let windows = ControlResponse::Windows(
        (0..WINDOWS)
            .map(|id| WindowInfo {
                id,
                app_id: "org.example.Terminal".to_owned(),
                title: "a title long enough to look like a real one".to_owned(),
                active: id == 0,
            })
            .collect(),
    );

    let mut encoded = Vec::new();
    send(&mut encoded, &windows).expect("encoding a window list cannot fail");
    let encoded = black_box(encoded);

    group.bench_function("list_encode", |b| {
        let mut out = Vec::with_capacity(encoded.len() + 8);
        b.iter(|| {
            out.clear();
            send(&mut out, &windows).expect("encoding a window list cannot fail");
        });
    });

    group.bench_function("list_decode", |b| {
        b.iter(|| {
            black_box(
                recv::<ControlResponse>(&mut encoded.as_slice())
                    .expect("decoding what send produced cannot fail"),
            )
        });
    });

    group.finish();
}

criterion_group!(benches, frames, input, control);
criterion_main!(benches);
