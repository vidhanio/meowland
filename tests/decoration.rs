//! Decoration policy checked against the real xdg-decoration wire protocol.
mod support;

use std::time::Duration;

use meowland::protocol::Show;
use support::{Client, Message, Pane, Server, hello, u32s};

/// Decoration configuration precedes the `xdg_surface` configure that applies
/// it.
fn configured(client: &mut Client, surface: u32, decoration: u32, initial: bool) {
    let events = client.sync();
    let modes: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.object == decoration && event.opcode == 0)
        .collect();
    assert!(
        !initial || !modes.is_empty(),
        "missing initial decoration configure"
    );
    for (_, event) in &modes {
        assert_eq!(
            event.u32_at(0),
            2,
            "client-side decoration mode was selected"
        );
    }
    let (index, configure): (usize, &Message) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| event.object == surface && event.opcode == 0)
        .expect("mode request must receive an xdg_surface.configure response");
    assert!(modes.iter().all(|(position, _)| *position < index));
    client.request(surface, 4, &u32s(&[configure.u32_at(0)]));
}

#[test]
fn decorations_are_server_side_even_when_the_client_prefers_titlebars() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let manager = client.bind("zxdg_decoration_manager_v1", 1);
    let surface = client.create_surface();
    let xdg_surface = client.alloc();
    client.request(client.xdg, 2, &u32s(&[xdg_surface, surface]));
    let toplevel = client.alloc();
    client.request(xdg_surface, 1, &u32s(&[toplevel]));
    let decoration = client.alloc();
    client.request(manager, 1, &u32s(&[decoration, toplevel]));
    // Negotiate decorations before the initial bufferless commit, as real
    // toolkits do, including an explicit preference for client-side chrome.
    client.request(decoration, 1, &u32s(&[1]));
    client.commit(surface);
    configured(&mut client, xdg_surface, decoration, true);

    // Requests are advisory; neither client-side mode nor an unset preference
    // should switch our policy. Each still needs a configure response.
    for (opcode, args) in [(1, u32s(&[1])), (1, u32s(&[2])), (2, Vec::new())] {
        client.request(decoration, opcode, &args);
        configured(&mut client, xdg_surface, decoration, false);
    }

    // No compositor chrome is added to the actual pixels delivered to panes.
    let buffer = client.shm_buffer(4, 3, 16, &[3, 2, 1, 255].repeat(12));
    client.attach_surface(surface, buffer, 4, 3);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let mut pane = Pane::attach(&server, hello(4, 3, Show::Newest));
    assert_eq!(pane.frame(), (4, 3, [1, 2, 3].repeat(12)));
}

#[test]
fn decoration_can_be_negotiated_after_the_initial_toplevel_configure() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("borderless", "meowland.test");
    let manager = client.bind("zxdg_decoration_manager_v1", 1);
    let decoration = client.alloc();
    client.request(manager, 1, &u32s(&[decoration, window.xdg_toplevel]));
    configured(&mut client, window.xdg_surface, decoration, true);
}
