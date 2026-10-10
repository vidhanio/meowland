//! Wire-codec benchmarks for full frames, input events, and window lists.
//! Encoding uses reusable output buffers; decoding includes message allocation.

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use meowland::protocol::{
    ControlResponse, Input, PaneToServer, ServerToPane, WindowInfo, modifiers, recv, send,
};

/// Pixel payloads at 1080p and 4K.
const SIZES: [(u32, u32, &str); 2] = [(1920, 1080, "1080p"), (3840, 2160, "2160p")];
const WINDOWS: u64 = 64;
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
        held: vec![56],
    });
    let pointer = PaneToServer::Input(Input::Pointer {
        x: 640.0,
        y: 360.0,
        button: None,
        pressed: false,
        scroll: 0,
    });

    for (name, message) in [("key", &key), ("pointer", &pointer)] {
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
