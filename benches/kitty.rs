//! Benchmarks for the pane-side frame encoder.
//!
//! `Presenter::present` is the whole of the CPU cost between a composed frame
//! and bytes on a pty, and the branch it takes is decided by what changed on
//! screen.  Each one is measured here: the first frame after an attach or a
//! resize, with flat and with incompressible content; a frame whose damage is
//! too large to patch; the cell-aligned patches a text pane mostly sends; a
//! frame that did not change at all; and the shared-memory handover a terminal
//! earns by proving that it reads one.
//!
//! The presenter and the frame handed to it are built in criterion's batched
//! setup, so a number is the encoder alone.  `cargo test --all-targets` runs
//! each case once, in criterion's test mode.
//!
//! A whole-frame case moves megabytes per iteration, so its absolute number
//! reflects the memory system (page faults, huge pages, allocator reuse) as
//! much as the encoder: the same case has been measured 2x apart in two
//! processes while producing identical bytes.  Compare whole-frame cases only
//! against runs of this same suite; the patch and codec cases are stable.
//! `cargo bench -- present/patches` narrows a run to one group.

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use meowland::kitty::{Presenter, SharedMemory};

/// One cell of a terminal showing a 1080p screen, as `CSI 14 t` reports it.
const CELL: (u16, u16) = (10, 20);
/// The flat colour the clients in these benchmarks draw.
const FLAT: u8 = 17;
/// A pixel value the base frames never contain.
const CHANGED: u8 = 0xab;
/// A laptop screen and a 4K one: on the second, compression and base64 cost
/// something a frame budget notices.
const SIZES: [(u32, u32, &str); 2] = [(1920, 1080, "1080p"), (3840, 2160, "2160p")];
/// The screen the patch benchmarks run at.  Patch size is a per-cell decision,
/// so one size shows what it costs.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const BYTES: usize = pixels(WIDTH, HEIGHT);
/// Fewer samples than criterion's default, and a shorter measurement: a 2160p
/// case already holds tens of milliseconds per iteration.
const SAMPLES: usize = 20;
const MEASUREMENT: Duration = Duration::from_secs(3);
const WARM_UP: Duration = Duration::from_secs(1);

const fn pixels(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3
}

/// xorshift, the generator the encoder tests use: deterministic, and byte for
/// byte incompressible, which is what video looks like to the compressor.
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

/// Fills the `w` by `h` pixel block at `(x, y)` with one value: a client
/// typing a glyph, or repainting a widget.
fn put_block(frame: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32, value: u8) {
    for row in 0..h {
        let start = ((y + row) * width + x) as usize * 3;
        frame[start..start + (w * 3) as usize].fill(value);
    }
}

/// One cell at the top-left corner changed.
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

/// Ten by twenty cells at `(100, 100)` changed: one patch of a hundred by four
/// hundred pixels, because the rows merge.
fn block(base: &[u8]) -> Vec<u8> {
    let mut frame = base.to_vec();
    put_block(&mut frame, WIDTH, 100, 100, 100, 400, CHANGED);
    frame
}

/// Thirty-two single cells spread over the screen: the most the encoder will
/// patch, and the diff has to walk every row to find them.
fn scattered(base: &[u8]) -> Vec<u8> {
    let mut frame = base.to_vec();
    for index in 0..32 {
        put_block(
            &mut frame,
            WIDTH,
            (index % 8) * 240,
            (index / 8) * 270,
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

        // A client drawing a solid screen: the sampling pass says compression
        // pays, so this is zlib plus base64 of what it produced.
        let flat = vec![FLAT; bytes];
        group.bench_function(BenchmarkId::new("flat", name), |b| {
            b.iter_batched(
                || (Presenter::new(Some(CELL), None), flat.clone()),
                |(mut presenter, frame)| black_box(presenter.present(width, height, frame)),
                BatchSize::LargeInput,
            );
        });

        // Incompressible pixels: the sampling pass has to refuse the full
        // compression pass, so this is base64 of the raw frame alone.
        let noisy = noise(&mut 9, bytes);
        group.bench_function(BenchmarkId::new("incompressible", name), |b| {
            b.iter_batched(
                || (Presenter::new(None, None), noisy.clone()),
                |(mut presenter, frame)| black_box(presenter.present(width, height, frame)),
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

    // A glyph being typed: the diff stops at the first differing row, and one
    // patch, one cursor move and one small payload go out.
    let cell = one_cell(&base);
    group.bench_function("one_cell_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                (presenter, cell.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    // A widget repainting, or a menu opening.
    let repainted = block(&base);
    group.bench_function("block_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                (presenter, repainted.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    // The patch budget spent exactly: thirty-two single-cell patches with their
    // deletes and cursor moves, and the four rows holding them are the only
    // ones scanned cell by cell.
    let spread = scattered(&base);
    group.bench_function("scattered_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                (presenter, spread.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    // A client redrawing what the terminal already shows: the patches that
    // covered the difference are deleted and no pixels move.
    group.bench_function("revert_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(Some(CELL), None);
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                black_box(presenter.present(WIDTH, HEIGHT, cell.clone()));
                (presenter, base.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    // Nothing changed: the frame is compared against the previous one and no
    // bytes are written at all.
    group.bench_function("unchanged_1080p", |b| {
        b.iter_batched(
            || {
                let mut presenter = Presenter::new(None, None);
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                (presenter, base.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn damage(c: &mut Criterion) {
    // The worst case the encoder can be asked for: the diff walks every row,
    // the changed area turns out to be the whole screen, and the frame goes
    // whole.  Anything that moves most of the screen — a scroll, a video
    // frame — is this case.
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
                black_box(presenter.present(WIDTH, HEIGHT, base.clone()));
                (presenter, noisy.clone())
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn shared_memory(c: &mut Criterion) {
    // The whole-frame path a terminal earns by proving it reads a shared
    // object: the pixels go into `/dev/shm` and the escape carries the name.
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

    // A terminal that reads each frame and unlinks the object: every frame can
    // be handed over.  A fresh presenter per iteration is the only way to model
    // the read from outside, because the object name belongs to the presenter.
    group.throughput(Throughput::Bytes(BYTES as u64));
    group.bench_function("transferred_1080p", |b| {
        b.iter_batched(
            || {
                (
                    Presenter::new(None, Some(SharedMemory::new())),
                    noisy.clone(),
                )
            },
            |(mut presenter, frame)| black_box(presenter.present(WIDTH, HEIGHT, frame)),
            BatchSize::LargeInput,
        );
    });

    // A terminal that has not read the object yet: the second frame has to go
    // over the pty instead, which is what keeps the two transfers in order.
    group.throughput(Throughput::Bytes((BYTES * 2) as u64));
    group.bench_function("unread_falls_back_1080p", |b| {
        b.iter_batched(
            || {
                (
                    Presenter::new(None, Some(SharedMemory::new())),
                    noisy.clone(),
                    other.clone(),
                )
            },
            |(mut presenter, first, second)| {
                black_box(presenter.present(WIDTH, HEIGHT, first));
                black_box(presenter.present(WIDTH, HEIGHT, second))
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, whole_frames, patches, damage, shared_memory);
criterion_main!(benches);
