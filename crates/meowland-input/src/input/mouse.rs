//! SGR pixel mouse report decoding with signed coordinates.

use meowland_protocol::{Button, Input, Scroll, Transition};

use super::event::Event;

pub(super) const MOUSE_PREFIX: &[u8] = b"\x1b[<";
pub(super) const MAX_MOUSE: usize = 64;
/// SGR bit marking a wheel report rather than a button or motion report.
const WHEEL: u32 = 64;
/// SGR bit marking pointer motion.
const MOTION: u32 = 32;
/// SGR bit marking buttons above the first three, reported as numbers from 8.
const EXTRA: u32 = 128;
/// First X11 button number carried through the [`EXTRA`] bit.
const EXTRA_BUTTON: u8 = 8;
/// Wheel distance the terminal reports for one detent.
const SCROLL_DETENT: i16 = 120;

pub(super) fn mouse(report: &[u8]) -> Option<Event> {
    let (&final_byte, fields) = report.strip_prefix(MOUSE_PREFIX)?.split_last()?;
    let released = match final_byte {
        b'M' => false,
        b'm' => true,
        _ => return None,
    };
    let mut fields = std::str::from_utf8(fields).ok()?.split(';');
    let code: u32 = fields.next()?.parse().ok()?;
    let x: i32 = fields.next()?.parse().ok()?;
    let y: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let (button, scroll) = if code & WHEEL != 0 {
        (None, wheel(code))
    } else if code & MOTION != 0 {
        (None, Scroll::default())
    } else {
        let state = if released {
            Transition::Released
        } else {
            Transition::Pressed
        };
        (Some((button(code)?, state)), Scroll::default())
    };
    Some(Event::Pointer(Input::Pointer {
        x: f64::from(x),
        y: f64::from(y),
        button,
        scroll,
    }))
}

/// Decode the wheel direction of a report with the [`WHEEL`] bit set.
fn wheel(code: u32) -> Scroll {
    let mut scroll = Scroll::default();
    match code & 3 {
        0 => scroll.vertical = SCROLL_DETENT,
        1 => scroll.vertical = -SCROLL_DETENT,
        2 => scroll.horizontal = -SCROLL_DETENT,
        _ => scroll.horizontal = SCROLL_DETENT,
    }
    scroll
}

/// Decode a button report into its X11 identity, rejecting reserved numbers.
fn button(code: u32) -> Option<Button> {
    let number = u8::try_from(code & 3).ok()?;
    if code & EXTRA != 0 {
        return Some(match number {
            0 => Button::Back,
            1 => Button::Forward,
            other => Button::Other(EXTRA_BUTTON + other),
        });
    }
    match number {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    pub(in crate::input) struct Pointer {
        pub(in crate::input) x: f64,
        pub(in crate::input) y: f64,
        pub(in crate::input) button: Option<(Button, Transition)>,
        pub(in crate::input) scroll: Scroll,
    }

    pub(in crate::input) fn pointer(event: &Event) -> Pointer {
        match event {
            Event::Pointer(Input::Pointer {
                x,
                y,
                button,
                scroll,
            }) => Pointer {
                x: *x,
                y: *y,
                button: *button,
                scroll: *scroll,
            },
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }

    #[test]
    fn mouse_decodes_buttons_and_signed_coordinates() {
        struct Case {
            report: &'static [u8],
            button: Option<(Button, Transition)>,
            x: f64,
            y: f64,
        }

        let cases = [
            Case {
                report: b"\x1b[<0;10;20M",
                button: Some((Button::Left, Transition::Pressed)),
                x: 10.0,
                y: 20.0,
            },
            Case {
                report: b"\x1b[<0;10;20m",
                button: Some((Button::Left, Transition::Released)),
                x: 10.0,
                y: 20.0,
            },
            Case {
                report: b"\x1b[<1;-5;-20M",
                button: Some((Button::Middle, Transition::Pressed)),
                x: -5.0,
                y: -20.0,
            },
            Case {
                report: b"\x1b[<2;0;0m",
                button: Some((Button::Right, Transition::Released)),
                x: 0.0,
                y: 0.0,
            },
            Case {
                report: b"\x1b[<128;1;2M",
                button: Some((Button::Back, Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<129;1;2M",
                button: Some((Button::Forward, Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<130;1;2M",
                button: Some((Button::Other(10), Transition::Pressed)),
                x: 1.0,
                y: 2.0,
            },
            Case {
                report: b"\x1b[<131;1;2m",
                button: Some((Button::Other(11), Transition::Released)),
                x: 1.0,
                y: 2.0,
            },
        ];
        for case in cases {
            let event = mouse(case.report).expect("valid mouse report");
            let pointer = pointer(&event);
            assert_eq!(pointer.x, case.x, "report {:?}", case.report);
            assert_eq!(pointer.y, case.y, "report {:?}", case.report);
            assert_eq!(pointer.button, case.button, "report {:?}", case.report);
            assert_eq!(
                pointer.scroll,
                Scroll::default(),
                "report {:?}",
                case.report
            );
        }
    }

    #[test]
    fn mouse_decodes_all_wheel_directions_and_motion() {
        let cases = [
            (
                b"\x1b[<64;1;2M",
                Scroll {
                    vertical: 120,
                    horizontal: 0,
                },
            ),
            (
                b"\x1b[<65;1;2M",
                Scroll {
                    vertical: -120,
                    horizontal: 0,
                },
            ),
            (
                b"\x1b[<66;1;2M",
                Scroll {
                    vertical: 0,
                    horizontal: -120,
                },
            ),
            (
                b"\x1b[<67;1;2M",
                Scroll {
                    vertical: 0,
                    horizontal: 120,
                },
            ),
        ];
        for (report, expected) in cases {
            let event = mouse(report).expect("valid wheel report");
            let pointer = pointer(&event);
            assert_eq!(pointer.button, None, "report {report:?}");
            assert_eq!(pointer.scroll, expected, "report {report:?}");
        }

        for report in [&b"\x1b[<32;7;8M"[..], &b"\x1b[<35;7;8m"[..]] {
            let event = mouse(report).expect("valid motion report");
            let pointer = pointer(&event);
            assert_eq!((pointer.x, pointer.y), (7.0, 8.0));
            assert_eq!(pointer.button, None);
            assert_eq!(pointer.scroll, Scroll::default());
        }
    }

    #[test]
    fn mouse_rejects_malformed_reports() {
        let cases: &[&[u8]] = &[
            b"",
            b"\x1b[<",
            b"\x1b[<0",
            b"\x1b[<0;1",
            b"\x1b[<0;1;2",
            b"\x1b[<0;1;2X",
            b"\x1b[<0;1;2;3M",
            b"\x1b[<a;1;2M",
            b"\x1b[<0;a;2M",
            b"\x1b[<0;1;999999999999M",
            b"\x1b[<3;1;2M",
            b"\x1b[<0;1;2M\xff",
        ];
        for report in cases {
            assert!(mouse(report).is_none(), "report {report:?}");
        }
        assert!(mouse(b"\x1b[<0;\xff;2M").is_none());
    }
}
