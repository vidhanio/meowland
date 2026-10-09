//! Kitty image layering must survive a multiplexer's host image-ID remapping.

mod support;

use meowland::kitty::{Presenter, SharedMemory};
use support::fake::FakeTerminal;

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
