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
    /// Returns false for partial bands, leaving the presenter unchanged.
    pub fn retain_frame(&mut self, band: FrameBand) -> bool {
        if band.y() != 0 || band.rgb().len() != band.size().rgb_len() {
            return false;
        }
        let (size, _, pixels) = band.into_parts();
        self.image = pixels;
        self.size = Some(size);
        self.displayed = true;
        true
    }

    /// Retain a validated row band and encode its terminal update.
    ///
    /// # Panics
    /// Panics if a validated band height does not fit in `u32`.
    pub fn present_into(&mut self, band: FrameBand, out: &mut Vec<u8>) -> PresentOutcome {
        out.clear();
        let (size, y, pixels) = band.into_parts();
        let Some(bytes) = size.rgb_band(y, pixels.len()) else {
            return PresentOutcome::NeedsKeyframe;
        };
        let whole = bytes.start == 0 && bytes.end == size.rgb_len();
        let matching = self.size == Some(size);
        if !whole && !matching {
            return PresentOutcome::NeedsKeyframe;
        }
        if matching && self.image[bytes.clone()] == pixels {
            return self.repaint_into(out);
        }
        let edit = matching && self.displayed && !whole;
        if edit {
            let height = u32::try_from(pixels.len() / size.rgb_stride())
                .expect("band height fits validated frame height");
            out.extend_from_slice(b"\x1b[?2026h");
            self.encoder.image(
                out,
                SCREEN_ID,
                (size.width(), height),
                &pixels,
                Placement::Edit { y },
            );
            out.extend_from_slice(b"\x1b[?2026l");
        }
        if whole {
            self.image = pixels;
            self.size = Some(size);
        } else {
            self.image[bytes].copy_from_slice(&pixels);
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

fn begin_image(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026h\x1b[2J\x1b[H");
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Placement {
    Whole,
    Edit { y: u32 },
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
        Placement::Edit { y } => write!(
            out,
            "a=f,f=24,s={width},v={height},i={id},r=1,x=0,y={y},X=1,q=2"
        ),
    }
    .expect("writing to Vec cannot fail");
}
