//! Pane frame-encoder benchmarks: whole frames, patches, partial bands, and
//! shared-memory handoff. Inputs and presenters are prepared outside timed
//! work. Whole-frame measurements are sensitive to memory-system variation.

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use meowland::kitty::{Presenter, SharedMemory};

/// Cell dimensions reported by `CSI 14 t`.
const CELL: (u16, u16) = (10, 20);
const FLAT: u8 = 17;
const CHANGED: u8 = 0xab;
const SIZES: [(u32, u32, &str); 2] = [(1920, 1080, "1080p"), (3840, 2160, "2160p")];
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const BYTES: usize = pixels(WIDTH, HEIGHT);
const SAMPLES: usize = 20;
const MEASUREMENT: Duration = Duration::from_secs(3);
const WARM_UP: Duration = Duration::from_secs(1);

const fn pixels(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3
}

/// Deterministic xorshift noise for incompressible frames.
fn noise(state: &mut u32, len: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(len);
    for _ in 0..len {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        frame.push(*state as u8);
    }
    frame
}

fn put_block(frame: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32, value: u8) {
    for row in 0..h {
        let start = ((y + row) * width + x) as usize * 3;
        frame[start..start + (w * 3) as usize].fill(value);
    }
}

fn one_cell(base: &[u8]) -> Vec<u8> {
    let mut frame = base.to_vec();
    put_block(
        &mut frame,
        WIDTH,
        0,
        0,
        u32::from(CELL.0),
        u32::from(CELL.1),
        CHANGED,
    );
    frame
}

/// A 100x400-pixel block that merges into one patch.
fn block(base: &[u8]) -> Vec<u8> {
    let mut frame = base.to_vec();
    put_block(&mut frame, WIDTH, 100, 100, 100, 400, CHANGED);
    frame
}

/// Thirty-two scattered cells, the encoder's patch limit.
fn scattered(base: &[u8]) -> Vec<u8> {
    let mut frame = base.to_vec();
    for index in 0..32 {
        put_block(
            &mut frame,
            WIDTH,
            (index % 8) * 240,
            (index / 8) * 260,
            u32::from(CELL.0),
            u32::from(CELL.1),
            CHANGED,
        );
    }
    frame
}

fn whole_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("present/whole_frame");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    for (width, height, name) in SIZES {
        let bytes = pixels(width, height);
        group.throughput(Throughput::Bytes(bytes as u64));

        let flat = vec![FLAT; bytes];
        group.bench_function(BenchmarkId::new("flat", name), |b| {
            b.iter_batched(
                || (Presenter::new(Some(CELL), None), flat.clone()),
                |(mut presenter, frame)| black_box(presenter.present(width, height, 0, frame)),
                BatchSize::LargeInput,
            );
        });

        let noisy = noise(&mut 9, bytes);
        group.bench_function(BenchmarkId::new("incompressible", name), |b| {
            b.iter_batched(
                || (Presenter::new(None, None), noisy.clone()),
                |(mut presenter, frame)| black_box(presenter.present(width, height, 0, frame)),
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

fn patches(c: &mut Criterion) {
    let mut group = c.benchmark_group("present/patches");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    let base = vec![FLAT; BYTES];

    // A single-cell patch after a full-row diff.
    let cell = one_cell(&base);
    group.bench_function("one_cell_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, cell.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    let repainted = block(&base);
    group.bench_function("block_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, repainted.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    // Exhaust the 32-patch budget.
    let spread = scattered(&base);
    group.bench_function("scattered_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, spread.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("revert_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                black_box(presenter.present(WIDTH, HEIGHT, 0, cell.clone()));
                (presenter, base.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("unchanged_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(None, None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, base.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn bands(c: &mut Criterion) {
    let mut group = c.benchmark_group("present/bands");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);
    let base = vec![FLAT; BYTES];
    let cell = one_cell(&base);

    // An unchanged band should not detach or resend the retained frame.
    let unchanged_band = base[..pixels(WIDTH, u32::from(CELL.1))].to_vec();
    group.bench_function("unchanged_band_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, unchanged_band.clone())
            },
            |(mut presenter, band)| black_box(presenter.present(WIDTH, HEIGHT, 0, band)),
            BatchSize::LargeInput,
        );
    });
    let cell_band = cell[..unchanged_band.len()].to_vec();
    group.bench_function("changed_band_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, cell_band.clone())
            },
            |(mut presenter, band)| black_box(presenter.present(WIDTH, HEIGHT, 0, band)),
            BatchSize::LargeInput,
        );
    });

    // Without a patch grid, update the retained frame in place.
    let changed_band = vec![CHANGED; unchanged_band.len()];
    group.bench_function("changed_band_no_grid_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(None, None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, changed_band.clone())
            },
            |(mut presenter, band)| black_box(presenter.present(WIDTH, HEIGHT, 0, band)),
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn damage(c: &mut Criterion) {
    // Full-screen damage after a row-by-row diff.
    let mut group = c.benchmark_group("present/damage");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP)
        .throughput(Throughput::Bytes(BYTES as u64));

    let base = vec![FLAT; BYTES];
    let noisy = noise(&mut 31, BYTES);
    group.bench_function("full_change_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, 0, base.clone()));
                (presenter, noisy.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn shared_memory(c: &mut Criterion) {
    // The shared-memory path writes pixels to an object and sends its name.
    let slot = SharedMemory::new();
    let usable = slot.probe().is_some();
    slot.clear();
    if !usable {
        eprintln!("skipping the shared-memory benchmarks: no usable /dev/shm");
        return;
    }

    let mut group = c.benchmark_group("present/shared_memory");
    group
        .sample_size(SAMPLES)
        .measurement_time(MEASUREMENT)
        .warm_up_time(WARM_UP);

    let noisy = noise(&mut 21, BYTES);
    let other = noise(&mut 22, BYTES);

    // A fresh presenter models each frame having an available shared object.
    group.throughput(Throughput::Bytes(BYTES as u64));
    group.bench_function("transferred_1080p", |b| {
        b.iter_batched(
            || {
                (
                    Presenter::new(None, Some(SharedMemory::new())),
                    noisy.clone(),
                )
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, 0, frame)),
            BatchSize::LargeInput,
        );
    });

    // An unread object causes the second frame to be dropped.
    group.throughput(Throughput::Bytes((BYTES * 2) as u64));
    group.bench_function("unread_drops_1080p", |b| {
        b.iter_batched(
            || {
                (
                    Presenter::new(None, Some(SharedMemory::new())),
                    noisy.clone(),
                    other.clone(),
                )
            },
            |(mut presenter, first, second)| {
                black_box(presenter.present(WIDTH, HEIGHT, 0, first));
                let dropped = presenter.present(WIDTH, HEIGHT, 0, second);
                assert!(dropped.is_empty() && presenter.dropped(), "not dropped");
                black_box(dropped)
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, whole_frames, patches, bands, damage, shared_memory);
criterion_main!(benches);
