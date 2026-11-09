//! Retained RGB frames with in-place Kitty root-frame updates.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedFrame};

use crate::encoding::{Encoder, Placement, transmit};

/// Kitty image identity of the pane's screen image.
pub const SCREEN_ID: u32 = 1;
/// Kitty image identity of the in-place frame edit capability probe.
pub const FRAME_EDIT_PROBE_ID: u32 = 30;
/// Kitty image identity of the shared-memory capability probe.
pub const SHARED_PROBE_ID: u32 = 32;

/// The pixels of the last whole frame with every later band applied.
#[derive(Debug)]
struct Retained {
    size: FrameSize,
    image: Vec<u8>,
}

/// What the terminal's placement currently shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Displayed {
    /// Nothing, or a placement a grid change invalidated.
    #[default]
    Nothing,
    /// Exactly the retained pixels.
    Retained,
    /// A shared frame shown by reference whose pixels are not retained.
    Shared,
}

/// Retains pixels for full-image repaint and edits changed bands in place.
#[derive(Debug, Default)]
pub struct Presenter {
    retained: Option<Retained>,
    displayed: Displayed,
    encoder: Encoder,
}

/// Whether a retained image was encoded, unchanged, or needs a new basis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    /// Retained pixels already match the displayed image.
    Unchanged,
    /// Terminal commands were appended to the output buffer.
    Encoded,
    /// No whole frame is retained to apply the update to or repaint from.
    NeedsKeyframe,
}

impl Presenter {
    /// Forget the displayed placement without discarding retained pixels.
    pub const fn invalidate(&mut self) {
        self.displayed = Displayed::Nothing;
    }

    /// Adopt the pixels of a complete frame shown by [`Self::present_shared`].
    ///
    /// A placement still showing that shared frame now shows the retained
    /// pixels, so later bands edit it in place. A placement invalidated since
    /// is replaced by the next update. Returns false for partial rectangles,
    /// leaving the presenter unchanged.
    pub fn retain_frame(&mut self, band: FrameBand) -> bool {
        if !band.is_whole() {
            return false;
        }
        self.retained = Some(Retained {
            size: band.size(),
            image: band.into_rgb(),
        });
        if self.displayed == Displayed::Shared {
            self.displayed = Displayed::Retained;
        } else {
            self.displayed = Displayed::Nothing;
        }
        true
    }

    /// Retain a validated RGB rectangle and append its terminal update to
    /// `out`.
    ///
    /// # Panics
    /// Panics if a validated rectangle does not fit in `usize` offsets.
    pub fn present_into(&mut self, band: FrameBand, out: &mut Vec<u8>) -> PresentOutcome {
        let size = band.size();
        if band.is_whole() {
            let showing = self.displayed == Displayed::Retained
                && self
                    .retained
                    .as_ref()
                    .is_some_and(|retained| retained.size == size && retained.image == band.rgb());
            if showing {
                return PresentOutcome::Unchanged;
            }
            self.retained = Some(Retained {
                size,
                image: band.into_rgb(),
            });
            self.invalidate();
            return self.repaint_into(out);
        }
        let Some(retained) = self
            .retained
            .as_mut()
            .filter(|retained| retained.size == size)
        else {
            return PresentOutcome::NeedsKeyframe;
        };
        let (x, y, width, height) = (band.x(), band.y(), band.width(), band.height());
        let row_bytes = usize::try_from(width).expect("validated band width fits usize") * 3;
        let offset = usize::try_from(y).expect("validated band row fits usize") * size.rgb_stride()
            + usize::try_from(x).expect("validated band column fits usize") * 3;
        let rows = || {
            band.rgb()
                .chunks_exact(row_bytes)
                .enumerate()
                .map(|(row, pixels)| (offset + row * size.rgb_stride(), pixels))
        };
        let changed =
            rows().any(|(start, pixels)| retained.image[start..start + row_bytes] != *pixels);
        if !changed && self.displayed == Displayed::Retained {
            return PresentOutcome::Unchanged;
        }
        for (start, pixels) in rows() {
            retained.image[start..start + row_bytes].copy_from_slice(pixels);
        }
        if self.displayed != Displayed::Retained {
            return self.repaint_into(out);
        }
        out.extend_from_slice(b"\x1b[?2026h");
        self.encoder.image(
            out,
            SCREEN_ID,
            (width, height),
            band.rgb(),
            Placement::Edit { x, y },
        );
        out.extend_from_slice(b"\x1b[?2026l");
        PresentOutcome::Encoded
    }

    /// Append a replacement of the displayed image from retained pixels, if
    /// the placement no longer shows them.
    pub fn repaint_into(&mut self, out: &mut Vec<u8>) -> PresentOutcome {
        if self.displayed != Displayed::Nothing {
            return PresentOutcome::Unchanged;
        }
        let Some(retained) = &self.retained else {
            return PresentOutcome::NeedsKeyframe;
        };
        begin_image(out);
        self.encoder.image(
            out,
            SCREEN_ID,
            (retained.size.width(), retained.size.height()),
            &retained.image,
            Placement::Whole,
        );
        out.extend_from_slice(b"\x1b[?2026l");
        self.displayed = Displayed::Retained;
        PresentOutcome::Encoded
    }

    /// Append commands displaying a complete immutable object without reading
    /// or copying its pixels, replacing the placement.
    ///
    /// The caller must retain the object's producer guard until the terminal
    /// opens it, and pass its pixels to [`Self::retain_frame`] before
    /// incremental edits or repainting.
    pub fn present_shared(&mut self, frame: &SharedFrame, out: &mut Vec<u8>) {
        begin_image(out);
        out.extend_from_slice(b"\x1b_G");
        transmit(
            out,
            SCREEN_ID,
            frame.size().width(),
            frame.size().height(),
            Placement::Whole,
        );
        out.extend_from_slice(b",t=s;");
        out.extend_from_slice(STANDARD.encode(frame.name()).as_bytes());
        out.extend_from_slice(b"\x1b\\\x1b[?2026l");
        self.displayed = Displayed::Shared;
    }
}

/// Query shared-memory support using a published RGB object; the terminal
/// answers under [`SHARED_PROBE_ID`].
#[must_use]
pub fn query_shared(frame: &SharedFrame) -> Vec<u8> {
    format!(
        "\x1b_Ga=q,f=24,s={},v={},i={SHARED_PROBE_ID},t=s;{}\x1b\\",
        frame.size().width(),
        frame.size().height(),
        STANDARD.encode(frame.name())
    )
    .into_bytes()
}

/// Query in-place frame edits by transmitting, editing, then deleting a
/// one-pixel image under [`FRAME_EDIT_PROBE_ID`].
#[must_use]
pub fn query_frame_edits() -> Vec<u8> {
    let id = FRAME_EDIT_PROBE_ID;
    format!(
        "\x1b_Ga=t,f=24,s=1,v=1,i={id},q=2;AAAA\x1b\\\
         \x1b_Ga=f,f=24,s=1,v=1,i={id},r=1;AAAA\x1b\\\
         \x1b_Ga=d,d=I,i={id},q=2;\x1b\\"
    )
    .into_bytes()
}

fn begin_image(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026h\x1b[2J\x1b[H");
}

#[cfg(test)]
mod tests {
    use meowland_protocol::SharedObject;

    use super::*;
    use crate::encoding::tests::decode_image;

    fn band(size: FrameSize, x: u32, y: u32, width: u32, pixels: Vec<u8>) -> FrameBand {
        FrameBand::new(size, x, y, width, pixels).expect("valid test band")
    }

    #[test]
    fn partial_updates_merge_and_repaint_retained_pixels() {
        let size = FrameSize::new(4, 3).expect("valid size");
        let original: Vec<u8> = (0..36).collect();
        let patch: Vec<u8> = (100..112).collect();
        let mut expected = original.clone();
        expected[15..21].copy_from_slice(&patch[..6]);
        expected[27..33].copy_from_slice(&patch[6..]);
        let mut presenter = Presenter::default();
        let mut output = Vec::new();

        assert_eq!(
            presenter.present_into(band(size, 1, 1, 2, patch.clone()), &mut output),
            PresentOutcome::NeedsKeyframe
        );
        assert_eq!(output, []);
        assert_eq!(
            presenter.present_into(band(size, 0, 0, 4, original.clone()), &mut output),
            PresentOutcome::Encoded
        );
        let (header, decoded, _) = decode_image(&output);
        assert!(header.contains("a=T"));
        assert_eq!(decoded, original);

        output.clear();
        assert_eq!(
            presenter.present_into(band(size, 1, 1, 2, patch.clone()), &mut output),
            PresentOutcome::Encoded
        );
        let (header, decoded, _) = decode_image(&output);
        assert!(header.contains("a=f") && header.contains("x=1,y=1"));
        assert_eq!(decoded, patch);
        assert_eq!(
            presenter.retained.as_ref().map(|retained| &retained.image),
            Some(&expected)
        );

        output.clear();
        assert_eq!(
            presenter.present_into(band(size, 1, 1, 2, patch), &mut output),
            PresentOutcome::Unchanged
        );
        assert_eq!(output, []);
        presenter.invalidate();
        assert_eq!(presenter.repaint_into(&mut output), PresentOutcome::Encoded);
        let (header, decoded, _) = decode_image(&output);
        assert!(header.contains("a=T"));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn invalidation_survives_shared_frame_adoption() {
        let size = FrameSize::new(3, 2).expect("valid size");
        let pixels: Vec<u8> = (0..18).collect();
        let (shared, _file) = SharedObject::create(size).expect("create shared descriptor");
        let mut presenter = Presenter::default();
        let mut output = Vec::new();
        presenter.present_shared(shared.frame(), &mut output);
        presenter.invalidate();
        assert!(presenter.retain_frame(band(size, 0, 0, 3, pixels.clone())));
        output.clear();
        assert_eq!(presenter.repaint_into(&mut output), PresentOutcome::Encoded);
        assert_eq!(decode_image(&output).1, pixels);

        presenter.present_shared(shared.frame(), &mut output);
        presenter.invalidate();
        assert!(presenter.retain_frame(band(size, 0, 0, 3, pixels.clone())));
        output.clear();
        let patch = vec![200, 201, 202];
        assert_eq!(
            presenter.present_into(band(size, 1, 1, 1, patch.clone()), &mut output),
            PresentOutcome::Encoded
        );
        let (header, decoded, _) = decode_image(&output);
        let mut expected = pixels;
        expected[12..15].copy_from_slice(&patch);
        assert!(header.contains("a=T"));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn a_frame_matching_stale_pixels_replaces_a_shared_placement() {
        let size = FrameSize::new(2, 1).expect("valid size");
        let first = vec![1; 6];
        let (shared, _file) = SharedObject::create(size).expect("create shared descriptor");
        let mut presenter = Presenter::default();
        let mut output = Vec::new();
        presenter.present_into(band(size, 0, 0, 2, first.clone()), &mut output);
        presenter.present_shared(shared.frame(), &mut output);

        output.clear();
        assert_eq!(
            presenter.present_into(band(size, 0, 0, 2, first.clone()), &mut output),
            PresentOutcome::Encoded,
            "the terminal shows the shared frame, not the retained pixels"
        );
        assert_eq!(decode_image(&output).1, first);
    }

    #[test]
    fn adopted_shared_pixels_accept_in_place_edits() {
        let size = FrameSize::new(2, 1).expect("valid size");
        let (shared, _file) = SharedObject::create(size).expect("create shared descriptor");
        let mut presenter = Presenter::default();
        let mut output = Vec::new();
        presenter.present_shared(shared.frame(), &mut output);
        assert!(presenter.retain_frame(band(size, 0, 0, 2, vec![1; 6])));

        output.clear();
        assert_eq!(
            presenter.present_into(band(size, 1, 0, 1, vec![9; 3]), &mut output),
            PresentOutcome::Encoded
        );
        assert!(decode_image(&output).0.contains("a=f"));
    }

    #[test]
    fn repainting_without_retained_pixels_needs_a_keyframe() {
        let mut presenter = Presenter::default();
        let mut output = Vec::new();
        assert_eq!(
            presenter.repaint_into(&mut output),
            PresentOutcome::NeedsKeyframe
        );
        assert_eq!(output, []);
    }
}
