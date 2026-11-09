//! Kitty keyboard and focus CSI decoding.

use meowland_protocol::{KEY_INSERT, Modifiers};
use vtparse::CsiParam;

use super::event::{Event, Focus, Key, Kind};

/// Kitty functional-key numbers for keys reported with legacy CSI finals.
const KEY_DELETE: u32 = 57349;
const KEY_LEFT: u32 = 57350;
const KEY_RIGHT: u32 = 57351;
const KEY_UP: u32 = 57352;
const KEY_DOWN: u32 = 57353;
const KEY_PAGE_UP: u32 = 57354;
const KEY_PAGE_DOWN: u32 = 57355;
const KEY_HOME: u32 = 57356;
const KEY_END: u32 = 57357;
const KEY_F1: u32 = 57364;
const KEY_F6: u32 = 57369;
const KEY_F11: u32 = 57374;
const KEY_KP_BEGIN: u32 = 57427;

/// Decode exactly one nonnegative CSI integer fitting in `u32`.
#[must_use]
pub fn number(params: &[CsiParam]) -> Option<u32> {
    match params {
        [CsiParam::Integer(value)] => u32::try_from(*value).ok(),
        _ => None,
    }
}

pub(super) fn csi(params: &[CsiParam], final_byte: u8) -> Option<Event> {
    if params.is_empty() {
        match final_byte {
            b'I' => return Some(Event::Focus(Focus::Gained)),
            b'O' => return Some(Event::Focus(Focus::Lost)),
            _ => {}
        }
    }
    let mut fields = params.split(|param| *param == CsiParam::P(b';'));
    let key = fields
        .next()?
        .split(|param| *param == CsiParam::P(b':'))
        .next()?;
    let mut modifier = fields
        .next()
        .unwrap_or(&[])
        .split(|param| *param == CsiParam::P(b':'));
    let bits = modifier
        .next()
        .filter(|params| !params.is_empty())
        .map_or(Some(1), number)?
        .checked_sub(1)?;
    let bits = u8::try_from(bits).ok()?;
    let modifiers = Modifiers::from_kitty(bits);
    let kind = match modifier
        .next()
        .filter(|params| !params.is_empty())
        .map_or(Some(1), number)?
    {
        1 => Kind::Press,
        2 => Kind::Repeat,
        3 => Kind::Release,
        _ => return None,
    };
    let text = if final_byte == b'u' {
        fields.next().and_then(committed_text)
    } else {
        None
    };
    let code = match final_byte {
        b'u' => number(key)?,
        b'A' => KEY_UP,
        b'B' => KEY_DOWN,
        b'C' => KEY_RIGHT,
        b'D' => KEY_LEFT,
        b'H' => KEY_HOME,
        b'F' => KEY_END,
        b'E' => KEY_KP_BEGIN,
        b'P' => KEY_F1,
        b'Q' => KEY_F1 + 1,
        b'R' => KEY_F1 + 2,
        b'S' => KEY_F1 + 3,
        b'~' => match number(key)? {
            2 => KEY_INSERT,
            3 => KEY_DELETE,
            5 => KEY_PAGE_UP,
            6 => KEY_PAGE_DOWN,
            1 | 7 => KEY_HOME,
            4 | 8 => KEY_END,
            KEY_KP_BEGIN => KEY_KP_BEGIN,
            n @ 11..=15 => KEY_F1 + n - 11,
            n @ 17..=21 => KEY_F6 + n - 17,
            n @ 23..=24 => KEY_F11 + n - 23,
            _ => return None,
        },
        _ => return None,
    };
    if code == 0 {
        return match kind {
            Kind::Press | Kind::Repeat => text.map(Event::Text),
            Kind::Release => None,
        };
    }
    Some(Event::Key(Key {
        code,
        modifiers,
        kind,
        text,
    }))
}

fn committed_text(params: &[CsiParam]) -> Option<String> {
    let mut text = String::new();
    for param in params.split(|param| *param == CsiParam::P(b':')) {
        let character = char::from_u32(number(param)?)?;
        if character.is_control() {
            return None;
        }
        text.push(character);
    }
    (!text.is_empty() && text.len() <= meowland_protocol::MAX_INPUT_TEXT).then_some(text)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::input) fn key(event: &Event) -> &Key {
        match event {
            Event::Key(key) => key,
            other => panic!("expected a key event, got {other:?}"),
        }
    }
    fn text_params(length: usize) -> Vec<CsiParam> {
        let mut params = vec![
            CsiParam::Integer(97),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b';'),
        ];
        for index in 0..length {
            if index > 0 {
                params.push(CsiParam::P(b':'));
            }
            params.push(CsiParam::Integer(97));
        }
        params
    }

    #[test]
    fn csi_decodes_focus_and_rejects_parameterized_focus() {
        assert!(matches!(csi(&[], b'I'), Some(Event::Focus(Focus::Gained))));
        assert!(matches!(csi(&[], b'O'), Some(Event::Focus(Focus::Lost))));
        assert!(csi(&[CsiParam::Integer(1)], b'I').is_none());
        assert!(csi(&[], b'Z').is_none());
    }

    #[test]
    fn csi_maps_every_kitty_modifier_bit() {
        let cases = [
            (0, Modifiers::empty()),
            (1, Modifiers::SHIFT),
            (2, Modifiers::ALT),
            (4, Modifiers::CONTROL),
            (8, Modifiers::SUPER),
            (16, Modifiers::HYPER),
            (32, Modifiers::META),
            (64, Modifiers::CAPS_LOCK),
            (128, Modifiers::NUM_LOCK),
            (3, Modifiers::SHIFT | Modifiers::ALT),
            (5, Modifiers::SHIFT | Modifiers::CONTROL),
        ];
        for (bits, expected) in cases {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(i64::from(bits) + 1),
            ];
            let event = csi(&params, b'u').expect("valid Kitty key event");
            let key = key(&event);
            assert_eq!(key.code, u32::from(b'a'), "Kitty bits {bits}");
            assert_eq!(key.modifiers, expected, "Kitty bits {bits}");
            assert_eq!(key.kind, Kind::Press, "Kitty bits {bits}");
            assert_eq!(key.text, None, "Kitty bits {bits}");
        }
    }

    #[test]
    fn csi_decodes_key_transitions() {
        let cases = [(1, Kind::Press), (2, Kind::Repeat), (3, Kind::Release)];
        for (encoded, expected) in cases {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b':'),
                CsiParam::Integer(encoded),
            ];
            let event = csi(&params, b'u').expect("valid Kitty transition");
            assert_eq!(key(&event).kind, expected, "encoded kind {encoded}");
        }
        for encoded in [0, 4, 99] {
            let params = [
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b':'),
                CsiParam::Integer(encoded),
            ];
            assert!(
                csi(&params, b'u').is_none(),
                "encoded kind {encoded} must be rejected"
            );
        }
    }

    #[test]
    fn csi_decodes_associated_text() {
        let cases: [(&[CsiParam], &str); 3] = [
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::Integer(1),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                ],
                "a",
            ),
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::Integer(1),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                    CsiParam::P(b':'),
                    CsiParam::Integer(98),
                ],
                "ab",
            ),
            (
                &[
                    CsiParam::Integer(97),
                    CsiParam::P(b';'),
                    CsiParam::P(b';'),
                    CsiParam::Integer(97),
                ],
                "a",
            ),
        ];
        for (params, expected) in cases {
            let event = csi(params, b'u').expect("valid Kitty key event");
            assert_eq!(key(&event).text.as_deref(), Some(expected));
        }
    }

    #[test]
    fn csi_bounds_associated_text() {
        let exact = text_params(meowland_protocol::MAX_INPUT_TEXT);
        let event = csi(&exact, b'u').expect("valid Kitty key event");
        assert_eq!(
            key(&event).text.as_deref().map(str::len),
            Some(meowland_protocol::MAX_INPUT_TEXT)
        );

        let oversized = text_params(meowland_protocol::MAX_INPUT_TEXT + 1);
        let event = csi(&oversized, b'u').expect("valid Kitty key event");
        assert_eq!(key(&event).text, None);
    }

    #[test]
    fn csi_rejects_invalid_associated_text() {
        let cases: [&[CsiParam]; 4] = [
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0x7f),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
                CsiParam::Integer(0x11_0000),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(1),
                CsiParam::P(b';'),
            ],
        ];
        for params in cases {
            let event = csi(params, b'u').expect("text is optional for key events");
            assert_eq!(key(&event).text, None, "params {params:?}");
        }
    }

    #[test]
    fn csi_turns_text_only_events_into_committed_text() {
        let params = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(matches!(csi(&params, b'u'), Some(Event::Text(text)) if text == "a"));

        let repeat = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b':'),
            CsiParam::Integer(2),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(matches!(csi(&repeat, b'u'), Some(Event::Text(text)) if text == "a"));

        let release = [
            CsiParam::Integer(0),
            CsiParam::P(b';'),
            CsiParam::Integer(1),
            CsiParam::P(b':'),
            CsiParam::Integer(3),
            CsiParam::P(b';'),
            CsiParam::Integer(97),
        ];
        assert!(csi(&release, b'u').is_none());
        assert!(csi(&[CsiParam::Integer(0)], b'u').is_none());
    }

    #[test]
    fn csi_decodes_functional_keys() {
        let csi_cases = [
            (b'A', 57352),
            (b'B', 57353),
            (b'C', 57351),
            (b'D', 57350),
            (b'H', 57356),
            (b'F', 57357),
            (b'E', 57427),
            (b'P', 57364),
            (b'Q', 57365),
            (b'R', 57366),
            (b'S', 57367),
        ];
        for (byte, expected) in csi_cases {
            let event = csi(&[], byte).expect("valid functional key");
            assert_eq!(key(&event).code, expected, "final byte {byte:?}");
        }

        let tilde_cases = [
            (1, 57356),
            (2, 57348),
            (3, 57349),
            (4, 57357),
            (5, 57354),
            (6, 57355),
            (7, 57356),
            (8, 57357),
            (11, 57364),
            (15, 57368),
            (17, 57369),
            (21, 57373),
            (23, 57374),
            (24, 57375),
            (57427, 57427),
        ];
        for (number, expected) in tilde_cases {
            let event = csi(&[CsiParam::Integer(number)], b'~').expect("valid tilde key");
            assert_eq!(key(&event).code, expected, "tilde {number}");
        }
        for number in [0, 9, 10, 16, 22, 25, 57426] {
            assert!(
                csi(&[CsiParam::Integer(number)], b'~').is_none(),
                "tilde {number} must be rejected"
            );
        }
    }

    #[test]
    fn csi_rejects_malformed_parameters() {
        let cases: [&[CsiParam]; 5] = [
            &[CsiParam::Integer(-1)],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(0),
            ],
            &[
                CsiParam::Integer(97),
                CsiParam::P(b';'),
                CsiParam::Integer(300),
            ],
            &[CsiParam::P(b';')],
            &[],
        ];
        for params in cases {
            assert!(csi(params, b'u').is_none(), "params {params:?}");
        }
        assert!(csi(&[], b'Z').is_none());
    }
}
