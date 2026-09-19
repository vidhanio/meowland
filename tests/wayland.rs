//! End-to-end checks over the real Wayland and pane protocols: a raw client
//! creates shm-backed xdg toplevels and the pane socket must deliver exact
//! pixels, frame callbacks and lifecycle events in return.

mod support;

use std::{
    thread,
    time::{Duration, Instant},
};

use meowland::protocol::{self, Hello, ServerToPane, Show};
use support::{Client, Pane, Server, hello, wait_for};

const WIDTH: u32 = 4;
const HEIGHT: u32 = 3;

/// An Argb8888 buffer with per-pixel alpha and a known B, G, R order.
fn pixels() -> (Vec<u8>, Vec<u8>) {
    let mut raw = Vec::new();
    let mut rgb = Vec::new();
    for index in 0..WIDTH * HEIGHT {
        let (red, green, blue) = (
            (index * 3) as u8,
            (index * 3 + 1) as u8,
            (index * 3 + 2) as u8,
        );
        let alpha = if index % 2 == 0 { 0xff } else { 0x40 };
        raw.extend_from_slice(&[blue, green, red, alpha]);
        rgb.extend_from_slice(&[red, green, blue]);
    }
    (raw, rgb)
}

#[test]
fn committed_shm_window_reaches_the_pane_byte_for_byte() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("precise pixels", "meowland.test");
    let (raw, rgb) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&toplevel, buffer, WIDTH, HEIGHT);

    let windows = wait_for(Duration::from_secs(5), || !server.list().is_empty());
    assert!(windows, "window was never announced");
    let listed = server.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "precise pixels");
    assert_eq!(listed[0].app_id, "meowland.test");

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    assert_eq!(pane.frame(), (WIDTH, HEIGHT, rgb));
}

#[test]
fn frame_callbacks_wait_for_the_pane_ack() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("frame clock", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&toplevel, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    let _ = pane.frame();
    // The pane holds the frame now.  While it does, the window is shown, so
    // the compositor may not hand its client another frame; a round trip
    // proves the callback was not ordered before the pane's acknowledgement.
    let callback = client.frame_callback(toplevel.surface);
    let seen = client.sync();
    assert!(
        seen.iter().all(|message| message.object != callback),
        "frame callback fired before the pane acknowledged"
    );

    pane.send(&protocol::PaneToServer::Ack);
    let done = client.read_until(|message| message.object == callback);
    assert_eq!(done.opcode, 0);
}

#[test]
fn closing_the_toplevel_releases_its_pane() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("closing", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&toplevel, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Id(1)));
    let _ = pane.frame();
    client.destroy_toplevel(&toplevel);
    match pane.recv() {
        ServerToPane::Release(reason) => assert_eq!(reason, "window closed"),
        other => panic!("expected release, got {other:?}"),
    }
    assert!(wait_for(Duration::from_secs(5), || server
        .list()
        .is_empty()));
}

#[test]
fn unsupported_pane_protocol_version_is_rejected() {
    let server = Server::start();
    let mut stream =
        std::os::unix::net::UnixStream::connect(server.runtime.join("meowland-pane.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut handshake = hello(640, 480, None, Show::Newest);
    handshake.version = protocol::VERSION + 1;
    protocol::send(&mut stream, &protocol::PaneToServer::Hello(handshake)).unwrap();
    match protocol::recv::<ServerToPane>(&mut stream).unwrap() {
        ServerToPane::Reject(reason) => assert!(reason.contains("version")),
        other => panic!("expected rejection, got {other:?}"),
    }
}

/// End-to-end frame rate: client commit -> compositor copy -> pane socket ->
/// pane ack, for full 1080p RGB frames.  Run with
/// `cargo test --release --test wayland -- --ignored --nocapture`.
#[test]
#[ignore = "manual performance check"]
fn throughput_of_1080p_frames() {
    use std::os::unix::fs::FileExt;

    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("bench", "meowland.bench");
    let (width, height) = (1920u32, 1080u32);
    let stride = width * 4;
    let pixels = vec![0x33; (stride * height) as usize];
    let (buffer, file) = client.shm_buffer_with_file(width, height, stride, &pixels);
    client.attach(&toplevel, buffer, width, height);
    assert!(wait_for(Duration::from_secs(10), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(width, height, Some((10, 20)), Show::Newest));
    let _ = pane.frame();
    pane.send(&protocol::PaneToServer::Ack);

    let budget = Duration::from_secs(5);
    let start = Instant::now();
    let (mut frames, mut bytes) = (0u64, 0u64);
    while start.elapsed() < budget {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if client
                .drain()
                .iter()
                .any(|message| message.object == buffer && message.opcode == 0)
            {
                break;
            }
            assert!(Instant::now() < deadline, "buffer was never released");
            thread::sleep(Duration::from_millis(1));
        }
        let offset = (frames * 4096) % u64::from(stride * height);
        file.write_at(&[0xa5], offset).unwrap();
        client.attach(&toplevel, buffer, width, height);
        let (frame_width, frame_height, _) = pane.frame();
        bytes += u64::from(frame_width) * u64::from(frame_height) * 3;
        pane.send(&protocol::PaneToServer::Ack);
        frames += 1;
    }
    let elapsed = start.elapsed();
    eprintln!(
        "1080p pane throughput: {frames} frames in {elapsed:?} ({:.1} fps, {:.0} MiB/s of RGB)",
        f64::from(frames as u32) / elapsed.as_secs_f64(),
        bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0),
    );
    assert!(frames > 30, "pipeline stalled: only {frames} frames");
}

#[test]
fn oversized_pane_hello_is_rejected() {
    let server = Server::start();
    let mut stream =
        std::os::unix::net::UnixStream::connect(server.runtime.join("meowland-pane.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let bad: Hello = Hello {
        version: protocol::VERSION,
        width: u32::MAX,
        height: 2,
        cell_width: None,
        cell_height: None,
        show: Show::Newest,
    };
    protocol::send(&mut stream, &protocol::PaneToServer::Hello(bad)).unwrap();
    assert!(matches!(
        protocol::recv::<ServerToPane>(&mut stream).unwrap(),
        ServerToPane::Reject(_)
    ));
}

/// An opaque ARGB8888 buffer of one colour.
fn solid(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::new();
    for _ in 0..width * height {
        raw.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0xff]);
    }
    raw
}

/// The colour of one pixel of a pane frame.
fn at(frame: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 3] {
    let (width, _, rgb) = frame;
    let index = ((y * width + x) * 3) as usize;
    [rgb[index], rgb[index + 1], rgb[index + 2]]
}

#[test]
fn subsurface_is_drawn_at_its_position() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("subsurface", "meowland.test");
    let background = [10, 20, 30];
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, background));
    client.attach(&window, buffer, WIDTH, HEIGHT);

    let child = client.create_surface();
    let subsurface = client.create_subsurface(child, window.surface);
    client.subsurface_position(subsurface, 1, 1);
    client.subsurface_desync(subsurface);
    let child_buffer = client.shm_buffer(2, 2, 8, &solid(2, 2, [200, 100, 50]));
    client.attach_surface(child, child_buffer, 2, 2);
    client.commit(window.surface);

    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    let frame = pane.frame();
    assert_eq!(at(&frame, 0, 0), background);
    assert_eq!(at(&frame, 1, 1), [200, 100, 50]);
    assert_eq!(at(&frame, 2, 2), [200, 100, 50]);
    assert_eq!(at(&frame, 3, 0), background);
}

#[test]
fn popup_is_drawn_where_the_window_geometry_puts_it() {
    const WIDE: u32 = 6;
    const TALL: u32 = 4;
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("popup", "meowland.test");
    // The window geometry starts at (2, 1) inside the surface, so a popup
    // anchored at (1, 1) of that geometry is drawn at (3, 2) of the surface.
    client.set_window_geometry(window.xdg_surface, 2, 1, 4, 2);
    let background = [10, 20, 30];
    let buffer = client.shm_buffer(WIDE, TALL, WIDE * 4, &solid(WIDE, TALL, background));
    client.attach(&window, buffer, WIDE, TALL);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let popup = client.create_popup(window.xdg_surface, (2, 2), (1, 1, 1, 1));
    let position = popup.map(&mut client, 2, 2, &solid(2, 2, [200, 100, 50]));
    assert_eq!(
        position,
        (1, 1),
        "the configure is in window geometry space"
    );

    let mut pane = Pane::attach(&server, hello(WIDE, TALL, None, Show::Newest));
    let frame = pane.frame();
    assert_eq!(at(&frame, 2, 1), background);
    assert_eq!(at(&frame, 3, 2), [200, 100, 50]);
    assert_eq!(at(&frame, 4, 3), [200, 100, 50]);

    // A press outside the popup dismisses it, and the pane stops drawing it.
    pane.send(&protocol::PaneToServer::Ack);
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Pointer {
        x: 0.5,
        y: 0.5,
        button: Some(0),
        pressed: true,
        scroll: 0,
    }));
    let done = client.read_until(|message| message.object == popup.handle);
    assert_eq!(done.opcode, 1, "xdg_popup.popup_done");
    let frame = pane.frame();
    assert_eq!(at(&frame, 3, 2), background);
}

#[test]
fn viewporter_scales_the_source_into_the_pane() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("viewport", "meowland.test");
    // Four source pixels, one per corner of the destination.
    let corners = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    let mut raw = Vec::new();
    for corner in corners {
        raw.extend_from_slice(&[corner[2], corner[1], corner[0], 0xff]);
    }
    let buffer = client.shm_buffer(2, 2, 8, &raw);
    let viewport = client.create_viewport(window.surface);
    // The whole 2x2 source, magnified to fill the 4x4 pane.
    client.viewport_source(viewport, 0.0, 0.0, 2.0, 2.0);
    client.viewport_destination(viewport, 4, 4);
    client.attach(&window, buffer, 2, 2);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(4, 4, None, Show::Newest));
    let frame = pane.frame();
    for (x, y, corner) in [
        (0, 0, corners[0]),
        (3, 0, corners[1]),
        (0, 3, corners[2]),
        (3, 3, corners[3]),
        (1, 1, corners[0]),
        (2, 2, corners[3]),
    ] {
        assert_eq!(at(&frame, x, y), corner, "destination pixel at {x},{y}");
    }
}

#[test]
fn title_changes_reach_the_pane_and_the_list() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("first", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    let _ = pane.frame();
    client.set_title(window.xdg_toplevel, "second");
    loop {
        match pane.recv() {
            ServerToPane::Title(title) => {
                assert_eq!(title, "second");
                break;
            }
            ServerToPane::Frame { .. } | ServerToPane::Cursor(_) => {}
            other => panic!("unexpected message {other:?}"),
        }
    }
    assert_eq!(server.list()[0].title, "second");
}

#[test]
fn keyboard_input_reaches_the_client() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("keys", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let seat = client.seat();
    let keyboard = client.get_keyboard(seat);
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    let _ = pane.frame();

    let enter = client.read_until(|message| message.object == keyboard && message.opcode == 1);
    assert_eq!(
        enter.u32_at(1),
        window.surface,
        "the pane's window has focus"
    );

    pane.send(&protocol::PaneToServer::Input(protocol::Input::Key {
        code: 16,
        pressed: true,
        modifiers: 0,
    }));
    let press = client.read_until(|message| message.object == keyboard && message.opcode == 3);
    assert_eq!(press.u32_at(2), 16, "evdev code, not the xkb one");
    assert_eq!(press.u32_at(3), 1, "pressed");

    pane.send(&protocol::PaneToServer::Input(protocol::Input::Key {
        code: 16,
        pressed: false,
        modifiers: 0,
    }));
    let release = client.read_until(|message| message.object == keyboard && message.opcode == 3);
    assert_eq!(release.u32_at(2), 16);
    assert_eq!(release.u32_at(3), 0, "released");
}

#[test]
fn pointer_input_reaches_the_client() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("pointer", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let seat = client.seat();
    let pointer = client.get_pointer(seat);
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Newest));
    let _ = pane.frame();

    pane.send(&protocol::PaneToServer::Input(protocol::Input::Pointer {
        x: 1.0,
        y: 1.0,
        button: Some(0),
        pressed: true,
        scroll: 0,
    }));
    let enter = client.read_until(|message| message.object == pointer && message.opcode == 0);
    assert_eq!(enter.u32_at(1), window.surface);
    assert_eq!(enter.u32_at(2), 1 << 8, "one pixel in, in fixed point");
    let button = client.read_until(|message| message.object == pointer && message.opcode == 3);
    assert_eq!(button.u32_at(2), 0x110, "BTN_LEFT");
    assert_eq!(button.u32_at(3), 1, "pressed");

    // Motion reaches the client in its own coordinates.
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Pointer {
        x: 2.0,
        y: 2.0,
        button: None,
        pressed: false,
        scroll: 0,
    }));
    let motion = client.read_until(|message| message.object == pointer && message.opcode == 2);
    assert_eq!((motion.u32_at(1), motion.u32_at(2)), (2 << 8, 2 << 8));

    // Releasing the button ends the press, and outside the window the
    // pointer leaves: clicks cannot land on a window the pane is not showing.
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Pointer {
        x: 2.0,
        y: 2.0,
        button: Some(0),
        pressed: false,
        scroll: 0,
    }));
    let release = client.read_until(|message| message.object == pointer && message.opcode == 3);
    assert_eq!(release.u32_at(3), 0, "released");
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Pointer {
        x: 100.0,
        y: 100.0,
        button: None,
        pressed: false,
        scroll: 0,
    }));
    let leave = client.read_until(|message| message.object == pointer && message.opcode == 1);
    assert_eq!(leave.opcode, 1);
}

#[test]
fn alt_q_asks_the_window_to_close_and_alt_w_detaches() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("bindings", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(wait_for(Duration::from_secs(5), || !server
        .list()
        .is_empty()));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, None, Show::Id(1)));
    let _ = pane.frame();

    // Alt+Q asks the shown window to close; the client decides what to do.
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Key {
        code: 16,
        pressed: true,
        modifiers: 0b0100,
    }));
    let close = client.read_until(|message| message.object == window.xdg_toplevel);
    assert_eq!(close.opcode, 0, "xdg_toplevel.close");

    // Alt+W detaches the pane and leaves the window running.
    pane.send(&protocol::PaneToServer::Input(protocol::Input::Key {
        code: 17,
        pressed: true,
        modifiers: 0b0100,
    }));
    match pane.recv() {
        ServerToPane::Release(reason) => assert_eq!(reason, "detached"),
        other => panic!("expected a release, got {other:?}"),
    }
    assert_eq!(server.list().len(), 1, "the window outlives the pane");
}
