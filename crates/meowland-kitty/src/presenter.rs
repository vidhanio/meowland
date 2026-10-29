//! Retained RGB frames with in-place Kitty root-frame updates.

use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedFrame};

use crate::encoding::Encoder;

const SCREEN_ID: u32 = 1;

/// Retains pixels for full-image repaint and edits changed bands in place.
#[derive(Debug, Default)]
pub struct Presenter {
    cell_size: Option<(u16, u16)>,
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
    /// Create a presenter for terminals supporting in-place Kitty frame edits.
    #[must_use]
    pub fn new(cell_size: Option<(u16, u16)>) -> Self {
        Self {
            cell_size,
            ..Self::default()
        }
    }

    /// Changing the grid requires replacing the image placement.
    pub fn set_cell_size(&mut self, cell_size: Option<(u16, u16)>) -> bool {
        if self.cell_size == cell_size {
            return false;
        }
        self.cell_size = cell_size;
        self.invalidate();
        true
    }

    /// Forget the displayed placement without discarding retained pixels.
    pub const fn invalidate(&mut self) {
        self.displayed = false;
    }

    /// Adopt a complete frame already displayed through an upstream transfer.
    ///
    /// Returns false for partial rectangles, leaving the presenter unchanged.
    pub fn retain_frame(&mut self, band: FrameBand) -> bool {
        if !band.is_whole() {
            return false;
        }
        let size = band.size();
        self.image = band.into_rgb();
        self.size = Some(size);
        self.displayed = true;
        true
    }

    /// Retain a validated RGB rectangle and encode its terminal update.
    ///
    /// # Panics
    /// Panics if a validated rectangle does not fit in `usize` offsets.
    pub fn present_into(&mut self, band: FrameBand, out: &mut Vec<u8>) -> PresentOutcome {
        out.clear();
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

    /// Replace the displayed image using retained pixels after a grid change.
    pub fn repaint_into(&mut self, out: &mut Vec<u8>) -> PresentOutcome {
        out.clear();
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
}

/// Encode a complete immutable object without reading or copying its pixels.
///
/// The caller must retain the object's producer guard until the terminal opens
/// it, and restore retained RGB pixels before incremental edits or repainting.
pub fn present_shared(frame: &SharedFrame, out: &mut Vec<u8>) {
    out.clear();
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

#[derive(Clone, Copy, Debug)]
pub(crate) enum Placement {
    Whole,
    Edit { x: u32, y: u32 },
}

impl Placement {
    pub(crate) const fn continuation(self) -> &'static [u8] {
        match self {
            Self::Whole => b"\x1b_G",
            Self::Edit { .. } => b"\x1b_Ga=f,",
        }
    }
}

/// Common Kitty transmission keys; callers append transport and chunk keys.
pub(crate) fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, placement: Placement) {
    match placement {
        Placement::Whole => write!(out, "a=T,f=24,s={width},v={height},i={id},p=1,z=1,C=1,q=2"),
        Placement::Edit { x, y } => write!(
            out,
            "a=f,f=24,s={width},v={height},i={id},r=1,x={x},y={y},X=1,q=2"
        ),
    }
    .expect("writing to Vec cannot fail");
}
