//! The pane's screen image with in-place Kitty root-frame edits.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedFrame};

use super::encoding::{Encoder, Placement, transmit};

/// Kitty image identity of the pane's screen image.
const SCREEN_ID: u32 = 1;
/// Kitty image identity of the in-place frame edit capability probe.
pub(crate) const FRAME_EDIT_PROBE_ID: u32 = 30;

const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
const END_SYNC: &[u8] = b"\x1b[?2026l";

/// Encodes frames as Kitty commands against one screen image.
///
/// The server owns every pixel. The presenter only remembers the size of the
/// whole image the terminal holds, so later bands can edit it in place.
#[derive(Debug, Default)]
pub(crate) struct Presenter {
    shown: Option<FrameSize>,
    encoder: Encoder,
}

impl Presenter {
    /// Forget the terminal's image, so bands wait for the next whole frame.
    pub(crate) const fn invalidate(&mut self) {
        self.shown = None;
    }

    /// Append the terminal update for a validated RGB rectangle to `out`.
    ///
    /// A whole frame replaces the placement. A partial rectangle edits the
    /// shown image in place; without a whole image of the same frame size to
    /// edit, nothing is appended and this returns false.
    pub(crate) fn present_into(&mut self, band: &FrameBand, out: &mut Vec<u8>) -> bool {
        let size = band.size();
        if band.is_whole() {
            out.extend_from_slice(BEGIN_SYNC);
            clear_screen(out);
            self.encoder.image(
                out,
                SCREEN_ID,
                (size.width(), size.height()),
                band.rgb(),
                Placement::Whole,
            );
            out.extend_from_slice(END_SYNC);
            self.shown = Some(size);
            return true;
        }
        if self.shown != Some(size) {
            return false;
        }
        out.extend_from_slice(BEGIN_SYNC);
        self.encoder.image(
            out,
            SCREEN_ID,
            (band.width(), band.height()),
            band.rgb(),
            Placement::Edit {
                x: band.x(),
                y: band.y(),
            },
        );
        out.extend_from_slice(END_SYNC);
        true
    }

    /// Append commands displaying a complete immutable object by reference,
    /// replacing the placement.
    ///
    /// The caller must keep the object alive until the terminal opens it.
    pub(crate) fn present_shared(&mut self, frame: &SharedFrame, out: &mut Vec<u8>) {
        out.extend_from_slice(BEGIN_SYNC);
        clear_screen(out);
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
        out.extend_from_slice(b"\x1b\\");
        out.extend_from_slice(END_SYNC);
        self.shown = Some(frame.size());
    }
}

/// Query in-place frame edits by transmitting, editing, then deleting a
/// one-pixel image under [`FRAME_EDIT_PROBE_ID`].
#[must_use]
pub(crate) fn query_frame_edits() -> Vec<u8> {
    let id = FRAME_EDIT_PROBE_ID;
    format!(
        "\x1b_Ga=t,f=24,s=1,v=1,i={id},q=2;AAAA\x1b\\\
         \x1b_Ga=f,f=24,s=1,v=1,i={id},r=1;AAAA\x1b\\\
         \x1b_Ga=d,d=I,i={id},q=2;\x1b\\"
    )
    .into_bytes()
}

fn clear_screen(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[2J\x1b[H");
}

#[cfg(test)]
mod tests {
    use meowland_protocol::SharedObject;

    use super::{super::encoding::tests::decode_image, *};

    fn band(size: FrameSize, x: u32, y: u32, width: u32, pixels: Vec<u8>) -> FrameBand {
        FrameBand::new(size, x, y, width, pixels).expect("valid test band")
    }

    #[test]
    fn bands_edit_only_a_whole_image_of_their_frame_size() {
        let size = FrameSize::new(4, 3).expect("valid size");
        let patch: Vec<u8> = (100..112).collect();
        let mut presenter = Presenter::default();
        let mut output = Vec::new();

        assert!(!presenter.present_into(&band(size, 1, 1, 2, patch.clone()), &mut output));
        assert_eq!(output, []);

        let whole: Vec<u8> = (0..36).collect();
        assert!(presenter.present_into(&band(size, 0, 0, 4, whole.clone()), &mut output));
        let (header, decoded, _) = decode_image(&output);
        assert!(header.contains("a=T"));
        assert_eq!(decoded, whole);

        output.clear();
        assert!(presenter.present_into(&band(size, 1, 1, 2, patch.clone()), &mut output));
        let (header, decoded, _) = decode_image(&output);
        assert!(header.contains("a=f") && header.contains("x=1,y=1"));
        assert_eq!(decoded, patch);

        output.clear();
        let other = FrameSize::new(4, 4).expect("valid size");
        assert!(!presenter.present_into(&band(other, 1, 1, 2, patch.clone()), &mut output));
        presenter.invalidate();
        assert!(!presenter.present_into(&band(size, 1, 1, 2, patch), &mut output));
        assert_eq!(output, []);
    }

    #[test]
    fn a_shared_frame_is_edited_in_place() {
        let size = FrameSize::new(2, 1).expect("valid size");
        let (shared, _file) = SharedObject::create(size).expect("create shared descriptor");
        let mut presenter = Presenter::default();
        let mut output = Vec::new();
        presenter.present_shared(shared.frame(), &mut output);

        output.clear();
        assert!(presenter.present_into(&band(size, 1, 0, 1, vec![9; 3]), &mut output));
        assert!(decode_image(&output).0.contains("a=f"));
    }
}
