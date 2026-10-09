//! Wheel delivery using the compositor's advertised pointer protocol.
mod support;

use std::time::Duration;

use meowland::protocol::{Input, PaneToServer, Show};
use support::{Client, Pane, Server, hello};

#[test]
fn wheel_axes_are_half_speed_and_have_their_own_frame() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("scroll", "meowland.test");
    let buffer = client.shm_buffer(8, 8, 32, &[3, 2, 1, 255].repeat(64));
    client.attach(&window, buffer, 8, 8);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let seat = client.seat();
    let pointer = client.get_pointer(seat);
    let mut pane = Pane::attach(&server, hello(8, 8, Show::Newest));
    let _ = pane.frame();

    // The first wheel establishes pointer focus. Repeated wheel events still
    // carry terminal coordinates even when the pointer has not moved.
    for scroll in [-120i16, 120, -120] {
        pane.send(&PaneToServer::Input(Input::Pointer {
            x: 4.0,
            y: 4.0,
            button: None,
            pressed: true,
            scroll,
        }));
        let mut group = Vec::new();
        loop {
            let event = client.read();
            if event.object != pointer {
                continue;
            }
            let opcode = event.opcode;
            group.push(event);
            if opcode == 5 {
                if group.iter().any(|event| event.opcode == 4) {
                    break;
                }
                group.clear();
            }
        }
        assert!(
            group.iter().all(|event| !matches!(event.opcode, 0..=3)),
            "pointer events mixed with scroll: {group:?}"
        );
        let axis = group.iter().find(|event| event.opcode == 4).unwrap();
        assert_eq!(axis.u32_at(1), 0, "vertical axis");
        // Half-speed distance: 7.5 units per click, in 24.8 fixed point.
        assert_eq!(axis.u32_at(2) as i32, -i32::from(scroll) * 16);
        let source = group
            .iter()
            .find(|event| event.opcode == 6)
            .expect("axis_source");
        assert_eq!(source.u32_at(0), 0, "wheel source");
        let ticks = group
            .iter()
            .find(|event| event.opcode == 9)
            .expect("axis_value120");
        assert_eq!(ticks.u32_at(0), 0, "vertical detents");
        assert_eq!(ticks.u32_at(1) as i32, -i32::from(scroll) / 2);
        assert_eq!(group.last().unwrap().opcode, 5, "axis frame terminator");
    }
    assert!(
        !client
            .sync()
            .iter()
            .any(|event| event.object == pointer && event.opcode == 4),
        "extra wheel update"
    );
    pane.send(&PaneToServer::Ack { drawn: true });
}
