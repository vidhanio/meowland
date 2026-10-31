//! Retained RGB frames with in-place Kitty root-frame updates.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedFrame};

use crate::encoding::{Encoder, Placement, transmit};

const SCREEN_ID: u32 = 1;

/// Retains pixels for full-image repaint and edits changed bands in place.
#[derive(Debug, Default)]
pub struct Presenter {
    image: Vec<u8>,
    size: Option<FrameSize>,
    displayed: bool,
    encoder: Encoder,
}

/// Whether a retained image was encoded, unchanged, or needs a new basis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    /// Retained pixels already match the displayed image.
    Unchanged,
    /// Terminal commands were appended to the output buffer.
    Encoded,
    /// A partial update arrived without a matching full-frame basis.
    NeedsKeyframe,
}

impl Presenter {
    /// Forget the displayed placement without discarding retained pixels.
    pub const fn invalidate(&mut self) {
        self.displayed = false;
    }

    /// Adopt the pixels of a complete frame shown by [`Self::present_shared`].
    ///
    /// The placement keeps its state: it stays valid unless something
    /// invalidated it after the shared frame was presented, in which case the
    /// next update replaces it. Returns false for partial rectangles, leaving
    /// the presenter unchanged.
    pub fn retain_frame(&mut self, band: FrameBand) -> bool {
        if !band.is_whole() {
            return false;
        }
        let size = band.size();
        self.image = band.into_rgb();
        self.size = Some(size);
        true
    }

    /// Retain a validated RGB rectangle and append its terminal update to
    /// `out`.
    ///
    /// # Panics
    /// Panics if a validated rectangle does not fit in `usize` offsets.
    pub fn present_into(&mut self, band: FrameBand, out: &mut Vec<u8>) -> PresentOutcome {
        let whole = band.is_whole();
        let size = band.size();
        let (x, y, width) = (band.x(), band.y(), band.width());
        let height = band.height();
        let pixels = band.into_rgb();
        let matching = self.size == Some(size);
        if !whole && !matching {
            return PresentOutcome::NeedsKeyframe;
        }
        let row_bytes = usize::try_from(width).expect("validated band width fits usize") * 3;
        let offset = usize::try_from(y).expect("validated band row fits usize") * size.rgb_stride()
            + usize::try_from(x).expect("validated band column fits usize") * 3;
        if matching
            && pixels
                .chunks_exact(row_bytes)
                .enumerate()
                .all(|(row, pixels)| {
                    let start = offset + row * size.rgb_stride();
                    self.image[start..start + row_bytes] == *pixels
                })
        {
            return self.repaint_into(out);
        }
        let edit = matching && self.displayed && !whole;
        if edit {
            out.extend_from_slice(b"\x1b[?2026h");
            self.encoder.image(
                out,
                SCREEN_ID,
                (width, height),
                &pixels,
                Placement::Edit { x, y },
            );
            out.extend_from_slice(b"\x1b[?2026l");
        }
        if whole {
            self.image = pixels;
            self.size = Some(size);
        } else {
            for (row, pixels) in pixels.chunks_exact(row_bytes).enumerate() {
                let start = offset + row * size.rgb_stride();
                self.image[start..start + row_bytes].copy_from_slice(pixels);
            }
        }
        if edit {
            PresentOutcome::Encoded
        } else {
            self.invalidate();
            self.repaint_into(out)
        }
    }

    /// Append a replacement of the displayed image from retained pixels, if the
    /// placement was invalidated.
    pub fn repaint_into(&mut self, out: &mut Vec<u8>) -> PresentOutcome {
        let Some(size) = self.size.filter(|_| !self.displayed) else {
            return PresentOutcome::Unchanged;
        };
        begin_image(out);
        self.encoder.image(
            out,
            SCREEN_ID,
            (size.width(), size.height()),
            &self.image,
            Placement::Whole,
        );
        out.extend_from_slice(b"\x1b[?2026l");
        self.displayed = true;
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
        self.displayed = true;
    }
}

/// Query shared-memory support using a published RGB object and request ID.
#[must_use]
pub fn query_shared(frame: &SharedFrame, id: u32) -> Vec<u8> {
    format!(
        "\x1b_Ga=q,f=24,s={},v={},i={id},t=s;{}\x1b\\",
        frame.size().width(),
        frame.size().height(),
        STANDARD.encode(frame.name())
    )
    .into_bytes()
}

/// Query in-place frame edits by transmitting, editing, then deleting a
/// one-pixel image under `id`.
#[must_use]
pub fn query_frame_edits(id: u32) -> Vec<u8> {
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
        assert_eq!(presenter.image, expected);

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
}
