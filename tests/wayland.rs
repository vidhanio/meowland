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

    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );
    let listed = server.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "precise pixels");
    assert_eq!(listed[0].app_id, "meowland.test");

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    assert_eq!(pane.frame(), (WIDTH, HEIGHT, rgb));
}

#[test]
fn frame_callbacks_continue_at_display_rate_without_pane_acks() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("frame clock", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&toplevel, buffer, WIDTH, HEIGHT);
    assert!(server.wait_for_window(Duration::from_secs(5)));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    let _ = pane.frame();
    // Never acknowledge the pane frame. Each callback commits a new request;
    // even a terminal that remains stalled must not freeze the Wayland client.
    let start = Instant::now();
    let mut times = Vec::new();
    for _ in 0..4 {
        let callback = client.frame_callback(toplevel.surface);
        let done = client.read_until(|message| message.object == callback);
        assert_eq!(done.opcode, 0);
        times.push(done.u32_at(0));
    }
    assert!(
        times.windows(2).all(|pair| pair[1] > pair[0]),
        "the callback clock did not advance: {times:?}"
    );
    assert!(
        start.elapsed() < Duration::from_millis(450),
        "callbacks were held behind the unacknowledged frame"
    );
}

#[test]
fn closing_the_toplevel_releases_its_pane() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("closing", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&toplevel, buffer, WIDTH, HEIGHT);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Id(server.list()[0].id)));
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
    let mut handshake = hello(640, 480, Show::Newest);
    handshake.version = protocol::VERSION + 1;
    protocol::send(&mut stream, &protocol::PaneToServer::Hello(handshake)).unwrap();
    match protocol::recv::<ServerToPane>(&mut stream).unwrap() {
        ServerToPane::Reject(reason) => assert!(reason.contains("version")),
        other => panic!("expected rejection, got {other:?}"),
    }
}

/// Measure the existing threaded transport at small and larger pane counts.
#[test]
#[ignore = "manual performance check"]
fn transport_fanout_cost() {
    for count in [1, 8, 64] {
        let server = Server::start_with_env(&[
            ("MEOWLAND_XWAYLAND", "off"),
            ("MEOWLAND_RENDER_NODE", "off"),
        ]);
        let started = Instant::now();
        let panes: Vec<_> = (0..count)
            .map(|_| Pane::attach(&server, hello(8, 8, Show::Newest)))
            .collect();
        let attach = started.elapsed();
        let mut samples = Vec::new();
        for _ in 0..40 {
            let started = Instant::now();
            assert!(matches!(
                server.control(&protocol::ControlRequest::Ping),
                protocol::ControlResponse::Ok
            ));
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let status = server.process_status();
        let resources: Vec<_> = status
            .lines()
            .filter(|line| line.starts_with("Threads:") || line.starts_with("VmRSS:"))
            .collect();
        eprintln!(
            "{count} panes: attach {attach:?}, control median {:?}, p95 {:?}, {}",
            samples[20],
            samples[38],
            resources.join(", ")
        );
        drop(panes);
    }
}

/// Isolate Wayland readiness latency from the 60Hz presentation clock.
#[test]
#[ignore = "manual performance check"]
fn idle_wayland_roundtrip_latency() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let mut samples = Vec::new();
    for _ in 0..200 {
        let started = Instant::now();
        client.sync();
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "idle Wayland roundtrip: median {:?}, p95 {:?}, max {:?}",
        samples[100], samples[190], samples[199]
    );
}

/// Run with `cargo test --release --test wayland -- --ignored --nocapture`.
#[test]
#[ignore = "manual performance check"]
fn throughput_of_1080p_frames() {
    use std::os::unix::fs::FileExt;

    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("bench", "meowland.bench");
    let (width, height) = (1920u32, 1080u32);
    let stride = width * 4;
    let mut pixels = solid(width, height, [0x33; 3]);
    let (buffer, file) = client.shm_buffer_with_file(width, height, stride, &pixels);
    client.attach(&toplevel, buffer, width, height);
    assert!(
        server.wait_for_window(Duration::from_secs(10)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(width, height, Show::Newest));
    let _ = pane.frame();
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

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
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[0] ^= 1;
        }
        file.write_all_at(&pixels, 0).unwrap();
        client.attach(&toplevel, buffer, width, height);
        let (frame_width, frame_height, _) = pane.frame();
        assert_eq!(pane.band(), (0, height));
        bytes += u64::from(frame_width) * u64::from(frame_height) * 3;
        pane.send(&protocol::PaneToServer::Ack { drawn: true });
        frames += 1;
    }
    let elapsed = start.elapsed();
    eprintln!(
        "1080p pane throughput: {frames} whole frames in {elapsed:?} ({:.1} fps, {:.2} MiB/s)",
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
fn subsurface_position_and_stacking_follow_parent_commits() {
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

    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    let frame = pane.frame();
    assert_eq!(at(&frame, 0, 0), background);
    assert_eq!(at(&frame, 1, 1), [200, 100, 50]);
    assert_eq!(at(&frame, 2, 2), [200, 100, 50]);
    assert_eq!(at(&frame, 3, 0), background);
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

    client.subsurface_below(subsurface, window.surface);
    client.commit(window.surface);
    assert_eq!(at(&pane.frame(), 1, 1), background);
}

#[test]
fn nested_subsurface_positions_do_not_overflow() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("extreme offsets", "meowland.test");
    let background = [10, 20, 30];
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, background));
    client.attach(&window, buffer, WIDTH, HEIGHT);
    let child = client.create_surface();
    let subsurface = client.create_subsurface(child, window.surface);
    client.subsurface_position(subsurface, i32::MAX, 0);
    client.subsurface_desync(subsurface);
    let buffer = client.shm_buffer(2, 2, 8, &solid(2, 2, [40, 50, 60]));
    client.attach_surface(child, buffer, 2, 2);
    let grandchild = client.create_surface();
    let nested = client.create_subsurface(grandchild, child);
    client.subsurface_position(nested, i32::MAX, 0);
    client.subsurface_desync(nested);
    let buffer = client.shm_buffer(4, 2, 16, &solid(4, 2, [70, 80, 90]));
    client.attach_surface(grandchild, buffer, 4, 2);
    client.commit(child);
    client.commit(window.surface);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    assert_eq!(at(&pane.frame(), 0, 0), background);
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

    client.subsurface_position(nested, -i32::MAX, 0);
    client.commit(grandchild);
    client.commit(child);
    client.commit(window.surface);
    assert_eq!(at(&pane.frame(), 0, 0), [70, 80, 90]);
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
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let popup = client.create_popup(window.xdg_surface, (2, 2), (1, 1, 1, 1));
    let position = popup.map(&mut client, 2, 2, &solid(2, 2, [200, 100, 50]));
    assert_eq!(
        position,
        (1, 1),
        "the configure is in window geometry space"
    );

    let mut pane = Pane::attach(&server, hello(WIDE, TALL, Show::Newest));
    let frame = pane.frame();
    assert_eq!(at(&frame, 2, 1), background);
    assert_eq!(at(&frame, 3, 2), [200, 100, 50]);
    assert_eq!(at(&frame, 4, 3), [200, 100, 50]);

    // A press outside the popup dismisses it, and the pane stops drawing it.
    pane.send(&protocol::PaneToServer::Ack { drawn: true });
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
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(4, 4, Show::Newest));
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
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
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
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let seat = client.seat();
    let keyboard = client.get_keyboard(seat);
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
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
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let seat = client.seat();
    let pointer = client.get_pointer(seat);
    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
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
    assert_eq!(leave.u32_at(1), window.surface, "pointer left the window");
}

#[test]
fn alt_q_asks_the_window_to_close_and_alt_w_detaches() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("bindings", "meowland.test");
    let (raw, _) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Id(server.list()[0].id)));
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

/// A popup's own window geometry sits inside its surface, and a pane draws a
/// surface from its own origin: a popup anchored at (1,1) of a window geometry
/// that starts at (2,1) has its *geometry* at (3,2) but its pixels start two
/// up and to the left of that.
#[test]
fn popup_is_drawn_from_the_window_geometry_the_client_set() {
    const WIDE: u32 = 6;
    const TALL: u32 = 4;
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("popup geometry", "meowland.test");
    client.set_window_geometry(window.xdg_surface, 2, 1, 4, 2);
    let background = [10, 20, 30];
    let buffer = client.shm_buffer(WIDE, TALL, WIDE * 4, &solid(WIDE, TALL, background));
    client.attach(&window, buffer, WIDE, TALL);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let popup = client.create_popup(window.xdg_surface, (4, 4), (1, 1, 1, 1));
    client.set_window_geometry(popup.xdg_surface, 2, 2, 2, 2);
    let position = popup.map(&mut client, 4, 4, &solid(4, 4, [200, 100, 50]));
    assert_eq!(
        position,
        (1, 1),
        "the configure is in window geometry space"
    );

    let mut pane = Pane::attach(&server, hello(WIDE, TALL, Show::Newest));
    let frame = pane.frame();
    assert_eq!(
        at(&frame, 1, 0),
        [200, 100, 50],
        "the popup's surface origin is (2,1) + (1,1) - (2,2)"
    );
    assert_eq!(
        at(&frame, 5, 3),
        background,
        "nothing is drawn two pixels past the popup's surface"
    );
}

/// Two panes with crossed aspect ratios: the buffer bound has to cover both
/// axes independently, or a buffer the client is configured for is refused and
/// the pane goes blank.
#[test]
fn a_pane_bound_covers_both_axes() {
    const WIDE_PANE: (u32, u32) = (200, 50);
    const TALL_PANE: (u32, u32) = (40, 400);
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("bounds", "meowland.test");
    let buffer = client.shm_buffer(
        WIDE_PANE.0,
        WIDE_PANE.1,
        WIDE_PANE.0 * 4,
        &solid(WIDE_PANE.0, WIDE_PANE.1, [1, 2, 3]),
    );
    client.attach(&window, buffer, WIDE_PANE.0, WIDE_PANE.1);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );
    let id = server.list()[0].id;

    let mut wide = Pane::attach(&server, hello(WIDE_PANE.0, WIDE_PANE.1, Show::Id(id)));
    let _ = wide.frame();
    wide.send(&protocol::PaneToServer::Ack { drawn: true });
    let mut tall = Pane::attach(&server, hello(TALL_PANE.0, TALL_PANE.1, Show::Id(id)));
    let _ = tall.frame();
    tall.send(&protocol::PaneToServer::Ack { drawn: true });

    // The client redraws at the second pane's size: 40 wide, 400 tall fits
    // the bound only when the height does not come from the widest pane.
    let painted = [9, 8, 7];
    let buffer = client.shm_buffer(
        TALL_PANE.0,
        TALL_PANE.1,
        TALL_PANE.0 * 4,
        &solid(TALL_PANE.0, TALL_PANE.1, painted),
    );
    client.attach(&window, buffer, TALL_PANE.0, TALL_PANE.1);

    let frame = tall.frame();
    assert_eq!((frame.0, frame.1), TALL_PANE);
    assert_eq!(at(&frame, 0, 0), painted, "the tall pane shows the redraw");
}

#[test]
fn an_unacked_pane_receives_only_the_latest_frame_after_its_ack() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("busy pane", "meowland.test");
    let first = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, [1, 2, 3]));
    client.attach(&window, first, WIDTH, HEIGHT);
    assert!(server.wait_for_window(Duration::from_secs(5)));

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    assert_eq!(at(&pane.frame(), 0, 0), [1, 2, 3]);
    let second = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, [4, 5, 6]));
    client.attach(&window, second, WIDTH, HEIGHT);
    let third = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, [9, 8, 7]));
    client.attach(&window, third, WIDTH, HEIGHT);
    let _ = client.sync();
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

    assert_eq!(
        at(&pane.frame(), 0, 0),
        [9, 8, 7],
        "the busy pane was sent an obsolete image"
    );
}

#[test]
fn a_small_change_reaches_the_pane_as_a_band() {
    const SIDE: u32 = 64;
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("bands", "meowland.test");
    let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &solid(SIDE, SIDE, [1, 2, 3]));
    client.attach(&window, buffer, SIDE, SIDE);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Newest));
    let frame = pane.frame();
    assert_eq!((frame.0, frame.1), (SIDE, SIDE));
    assert_eq!(pane.band(), (0, SIDE), "the first frame is the whole of it");
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

    let mut raw = solid(SIDE, SIDE, [1, 2, 3]);
    for pixel in raw[..(SIDE * 4) as usize].as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[9, 9, 9, 0xff]);
    }
    let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &raw);
    client.attach(&window, buffer, SIDE, SIDE);

    let frame = pane.frame();
    let (y, rows) = pane.band();
    assert!(rows < SIDE, "a one row change sent {rows} rows");
    assert!(y + rows >= 1, "the changed row was not in the band");
    assert_eq!(at(&frame, 0, 0), [9, 9, 9], "the change is in the picture");
    assert_eq!(at(&frame, 0, SIDE - 1), [1, 2, 3], "the rest is untouched");
}

#[test]
fn resizing_an_unacked_pane_with_equal_pixel_area_sends_a_full_frame() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("resize", "meowland.test");
    let buffer = client.shm_buffer(8, 8, 32, &solid(8, 8, [7, 8, 9]));
    client.attach(&window, buffer, 8, 8);
    assert!(server.wait_for_window(Duration::from_secs(5)));

    let mut pane = Pane::attach(&server, hello(8, 4, Show::Newest));
    let first = pane.frame();
    assert_eq!((first.0, first.1), (8, 4));
    pane.send(&protocol::PaneToServer::Resize {
        width: 4,
        height: 8,
    });
    pane.send(&protocol::PaneToServer::Ack { drawn: true });
    let resized = pane.frame();
    assert_eq!((resized.0, resized.1), (4, 8));
    assert_eq!(pane.band(), (0, 8));
    assert_eq!(resized.2, [7, 8, 9].repeat(32));
}

/// A pane that stops reading and comes back later must still be connected: a
/// loaded machine can leave a pane unrun for seconds, and the frame only has
/// to wait for it.  A write that gives up on the pane ends a session that
/// needed nothing but time.
#[test]
fn a_pane_that_stalls_mid_frame_is_not_disconnected() {
    // The frame has to be far larger than the socket buffers, so the write
    // cannot finish while the pane is not reading.
    const SIDE: u32 = 2048;
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("slow reader", "meowland.test");
    let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &solid(SIDE, SIDE, [4, 5, 6]));
    client.attach(&window, buffer, SIDE, SIDE);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Newest));
    // Long enough that a write which gives up on the pane has given up: the
    // socket fills, the writer waits, and the pane comes back to the frame
    // that was waiting for it.
    thread::sleep(Duration::from_millis(5000));
    let frame = pane.frame();
    assert_eq!((frame.0, frame.1), (SIDE, SIDE));
    assert_eq!(at(&frame, 0, 0), [4, 5, 6], "the stalled frame was lost");
}

/// A pane that follows the newest window has to follow the one that is left
/// when that window closes, rather than sitting on an empty, black pane.
#[test]
fn a_following_pane_moves_to_the_next_window_when_the_newest_closes() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let first = client.create_toplevel("first", "meowland.test");
    let (raw, painted) = pixels();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &raw);
    client.attach(&first, buffer, WIDTH, HEIGHT);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "the first window was never announced"
    );

    let second = client.create_toplevel("second", "meowland.test");
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &solid(WIDTH, HEIGHT, [7, 7, 7]));
    client.attach(&second, buffer, WIDTH, HEIGHT);
    assert!(
        wait_for(Duration::from_secs(5), || server.list().len() == 2),
        "the second window was never announced"
    );

    let mut pane = Pane::attach(&server, hello(WIDTH, HEIGHT, Show::Newest));
    let frame = pane.frame();
    assert_eq!(at(&frame, 0, 0), [7, 7, 7], "the pane follows the newest");
    pane.send(&protocol::PaneToServer::Ack { drawn: true });

    client.destroy_toplevel(&second);
    let frame = pane.frame();
    assert_eq!(
        at(&frame, 0, 0),
        [painted[0], painted[1], painted[2]],
        "the pane should be showing the window that is left"
    );
}
