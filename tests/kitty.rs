//! Kitty image layering must survive a multiplexer's host image-ID remapping.

mod support;

use meowland::kitty::{Presenter, SharedMemory};
use support::fake::FakeTerminal;

fn paint(frame: &mut [u8], width: usize, x: usize, y: usize, w: usize, h: usize, value: u8) {
    for row in y..y + h {
        let start = (row * width + x) * 3;
        frame[start..start + w * 3].fill(value);
    }
}

fn paint_noise(frame: &mut [u8], width: usize, value: u8) {
    let region = noise(40 * 40 * 3);
    for row in 0..40usize {
        let start = ((row + 2) * width + 2) * 3;
        for (pixel, source) in frame[start..start + 40 * 3]
            .iter_mut()
            .zip(&region[row * 40 * 3..(row + 1) * 40 * 3])
        {
            *pixel = source ^ value;
        }
    }
}

#[test]
fn unchanged_patch_slots_are_not_retransmitted() {
    for shared in [false, true] {
        let mut presenter = Presenter::new(Some((2, 2)), shared.then(SharedMemory::new));
        let mut terminal = FakeTerminal::new(128, 128, (2, 2));
        terminal.remap_image_ids();
        let base = noise(128 * 128 * 3);
        terminal.feed(&presenter.present(128, 128, 0, base.clone()));
        let mut previous = base.clone();
        paint_noise(&mut previous, 128, 71);
        paint(&mut previous, 128, 100, 2, 2, 2, 93);
        terminal.feed(&presenter.present(128, 128, 0, previous.clone()));
        assert_eq!(terminal.patches, 2);

        let mut next = base.clone();
        paint_noise(&mut next, 128, 71);
        paint(&mut next, 128, 104, 2, 2, 2, 93);
        let bytes = presenter.present(128, 128, 0, next.clone());
        for fragment in bytes.chunks(11) {
            terminal.feed(fragment);
        }
        assert_eq!(terminal.screen(), next);
        assert_eq!(terminal.patches, 3, "only the moved slot should upload");

        // Same geometry, different pixels must still replace the slot.
        paint_noise(&mut next, 128, 72);
        terminal.feed(&presenter.present(128, 128, 0, next.clone()));
        assert_eq!(terminal.screen(), next);
        assert_eq!(terminal.patches, 5);

        // Shrinking the patch list must delete the now-unused cursor slot.
        let mut remaining = base.clone();
        paint_noise(&mut remaining, 128, 72);
        terminal.feed(&presenter.present(128, 128, 0, remaining.clone()));
        assert_eq!(terminal.screen(), remaining);
        assert_eq!(terminal.patches, 5);
        terminal.feed(&presenter.present(128, 128, 0, base.clone()));
        assert_eq!(terminal.screen(), base);
        assert_eq!(terminal.whole_frames, 1);
    }
}

#[test]
fn retained_noisy_patch_reduces_wire_bytes_and_resize_invalidates_it() {
    for shared in [false, true] {
        let mut presenter = Presenter::new(Some((2, 2)), shared.then(SharedMemory::new));
        let mut terminal = FakeTerminal::new(128, 128, (2, 2));
        let base = vec![0; 128 * 128 * 3];
        terminal.feed(&presenter.present(128, 128, 0, base.clone()));
        let mut previous = base.clone();
        let region = noise(40 * 40 * 3);
        for row in 0..40usize {
            let start = ((row + 2) * 128 + 2) * 3;
            previous[start..start + 40 * 3]
                .copy_from_slice(&region[row * 40 * 3..(row + 1) * 40 * 3]);
        }
        paint(&mut previous, 128, 100, 2, 2, 2, 93);
        let initial = presenter.present(128, 128, 0, previous.clone());
        terminal.feed(&initial);
        let mut next = previous;
        paint(&mut next, 128, 100, 2, 2, 2, 0);
        paint(&mut next, 128, 104, 2, 2, 2, 93);
        let retained = presenter.present(128, 128, 0, next.clone());
        terminal.feed(&retained);
        assert_eq!(terminal.screen(), next);
        assert!(retained.len() * 50 < initial.len());
        println!(
            "shared={shared}: initial={} bytes, retained={} bytes",
            initial.len(),
            retained.len()
        );

        // A smaller frame must remove all old patches, including cached ones.
        terminal.feed(&presenter.present(64, 64, 0, vec![17; 64 * 64 * 3]));
        let mut expected = base.clone();
        paint(&mut expected, 128, 0, 0, 64, 64, 17);
        assert_eq!(terminal.screen(), expected);
        terminal.feed(&presenter.present(128, 128, 0, base));
        terminal.feed(&presenter.present(128, 128, 0, next.clone()));
        assert_eq!(terminal.screen(), next);
        assert_eq!(terminal.whole_frames, 3);
    }
}

#[test]
fn patch_cache_survives_slot_reordering_and_whole_frame_refreshes() {
    for shared in [false, true] {
        let mut presenter = Presenter::new(Some((2, 2)), shared.then(SharedMemory::new));
        let mut terminal = FakeTerminal::new(128, 128, (2, 2));
        terminal.remap_image_ids();
        let mut base = noise(128 * 128 * 3);
        terminal.feed(&presenter.present(128, 128, 0, base.clone()));
        for step in 0..120usize {
            let mut next = base.clone();
            paint_noise(&mut next, 128, 91);
            paint(
                &mut next,
                128,
                (step % 16) * 2,
                (step / 16 % 16) * 2,
                2,
                2,
                step as u8,
            );
            if step % 17 == 0 {
                // Force a whole refresh, invalidating every retained slot.
                next.fill(step as u8);
                base = next.clone();
            }
            if step % 23 == 0 {
                presenter.set_cell_size(Some((2, 2)));
                presenter.set_cell_size(None);
                presenter.set_cell_size(Some((2, 2)));
            }
            terminal.feed(&presenter.present(128, 128, 0, next.clone()));
            assert_eq!(terminal.screen(), next, "shared={shared}, step={step}");
        }
    }
}

fn noise(len: usize) -> Vec<u8> {
    let mut seed = 1u32;
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect()
}

#[test]
fn patches_stay_above_the_base_when_host_image_ids_have_a_different_order() {
    for shared in [false, true] {
        let mut presenter = Presenter::new(Some((2, 2)), shared.then(SharedMemory::new));
        let mut terminal = FakeTerminal::new(8, 8, (2, 2));
        terminal.remap_image_ids();
        let base = noise(8 * 8 * 3);
        terminal.feed(&presenter.present(8, 8, 0, base.clone()));
        assert_eq!(terminal.screen(), base);

        // Moving small changes around a patterned base models scrolling and
        // repaints without hiding the bug behind a fresh whole-frame transfer.
        for step in 0..32 {
            let mut updated = base.clone();
            for (x, y) in [(0, 0), ((step % 3 + 1) * 2, (step / 3 % 3 + 1) * 2)] {
                for row in y..y + 2 {
                    for col in x..x + 2 {
                        let offset = (row * 8 + col) * 3;
                        updated[offset..offset + 3].copy_from_slice(&[step as u8, 0x70, 0x90]);
                    }
                }
            }
            terminal.feed(&presenter.present(8, 8, 0, updated.clone()));
            assert_eq!(terminal.screen(), updated, "shared={shared}, step={step}");
            assert_eq!(
                terminal.whole_frames, 1,
                "must exercise patches, not full refreshes"
            );
        }
        terminal.feed(&presenter.present(8, 8, 0, base.clone()));
        assert_eq!(
            terminal.screen(),
            base,
            "removing patches must reveal the base"
        );
        assert_eq!(terminal.whole_frames, 1);
        assert!(terminal.patches >= 32);
    }
}

#[test]
fn chunked_patches_from_partial_row_bands_keep_their_layer() {
    for shared in [false, true] {
        let mut presenter = Presenter::new(Some((40, 40)), shared.then(SharedMemory::new));
        let mut terminal = FakeTerminal::new(128, 128, (40, 40));
        terminal.remap_image_ids();
        let base = noise(128 * 128 * 3);
        terminal.feed(&presenter.present(128, 128, 0, base.clone()));
        let mut updated = base;
        for row in 40..80 {
            for col in 40..80 {
                let offset = (row * 128 + col) * 3;
                updated[offset] ^= 0xff;
            }
        }
        let band = updated[40 * 128 * 3..80 * 128 * 3].to_vec();
        let bytes = presenter.present(128, 128, 40, band);
        assert!(bytes.windows(7).any(|chunk| chunk == b"\x1b_Gm=0;"));
        // Fragment the transfer too: continuation chunks must retain the layer
        // from the first command until the complete patch is placed.
        for fragment in bytes.chunks(113) {
            terminal.feed(fragment);
        }
        let mismatch = terminal
            .screen()
            .iter()
            .zip(&updated)
            .position(|(actual, expected)| actual != expected);
        assert_eq!(mismatch, None, "shared={shared}: first stale byte");
        assert_eq!((terminal.whole_frames, terminal.patches), (1, 1));
    }
}
