//! The pane end to end: a real `meowland attach` process on a pty, a
//! simulated kitty terminal on the other end, and pixel comparisons of what
//! the pane draws.

mod support;

use std::{
    io::{Read as _, Write as _},
    os::unix::{fs::FileExt as _, net::UnixListener},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use support::{BINARY, Client, Pty, PtyChild, Server, fake::FakeTerminal, temp_dir, wait_for};

/// Four cells each way at 2x2 pixels: a one-cell change stays well inside the
/// quarter of the frame that still goes out as a patch.
const SIDE: u32 = 8;

fn gradient() -> Vec<u8> {
    let mut rgb = Vec::new();
    for index in 0..SIDE * SIDE {
        rgb.extend_from_slice(&[(index * 7) as u8, (index * 11) as u8, (index * 13) as u8]);
    }
    rgb
}

/// Rows are B, G, R, alpha in an Argb8888 buffer.
fn raw_from_rgb(rgb: &[u8]) -> Vec<u8> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 0xff])
        .collect()
}

/// Drive the pane until it has drawn `expected`, feeding the terminal replies.
fn pump_until_drawn(
    pty: &mut Pty,
    terminal: &mut FakeTerminal,
    child: &mut PtyChild,
    expected: &[u8],
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let bytes = pty.read_now();
        if !bytes.is_empty() {
            let replies = terminal.feed(&bytes);
            if !replies.is_empty() {
                pty.master.write_all(&replies).unwrap();
            }
        }
        if terminal.screen() == expected {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "pane exited before drawing {what}"
        );
        thread::sleep(Duration::from_millis(2));
    }
    panic!("pane never drew {what}: screen {:?}", terminal.screen());
}

#[test]
fn pane_draws_whole_and_patch_frames_then_detaches() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("pane test", "meowland.test");
    let rgb = gradient();
    let (buffer, file) = client.shm_buffer_with_file(SIDE, SIDE, SIDE * 4, &raw_from_rgb(&rgb));
    client.attach(&toplevel, buffer, SIDE, SIDE);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pty = Pty::open(4, 4, (2, 2));
    let mut child = pty.spawn(
        Command::new(BINARY)
            .args(["attach", "1"])
            .env("XDG_RUNTIME_DIR", &server.runtime),
    );
    let mut terminal = FakeTerminal::new(SIDE as usize, SIDE as usize, (2, 2));
    pump_until_drawn(&mut pty, &mut terminal, &mut child, &rgb, "the first frame");

    assert!(
        terminal.modes.contains("1016h"),
        "SGR-Pixels was not enabled: {:?}",
        terminal.modes
    );
    assert!(terminal.synchronized_updates >= 1, "no synchronized update");
    assert_eq!(
        terminal.shared_frames, 1,
        "the whole frame should have travelled through shared memory"
    );

    // A patch: change one 2x2 cell and expect the terminal's pixels to follow.
    let mut patched = rgb.clone();
    for pixel in 0..2 {
        for row in 0..2 {
            let offset = ((row * SIDE + pixel) * 3) as usize;
            patched[offset..offset + 3].copy_from_slice(&[0x10, 0x20, 0x30]);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
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
    for row in 0..2u64 {
        for column in 0..2u64 {
            let offset = (row * u64::from(SIDE) + column) * 4;
            let pixel = [0x30, 0x20, 0x10, 0xff];
            file.write_at(&pixel, offset).unwrap();
        }
    }
    client.attach(&toplevel, buffer, SIDE, SIDE);
    pump_until_drawn(
        &mut pty,
        &mut terminal,
        &mut child,
        &patched,
        "the patch frame",
    );
    assert_eq!(
        (terminal.whole_frames, terminal.patches),
        (1, 1),
        "the second frame should be a single patch over the first image"
    );
    let mut two_patches = patched;
    for row in 6..8 {
        for column in 6..8 {
            let offset = ((row * SIDE + column) * 3) as usize;
            two_patches[offset..offset + 3].copy_from_slice(&[0x40, 0x50, 0x60]);
        }
    }
    let mut bottom_only = rgb.clone();
    bottom_only[(6 * SIDE * 3) as usize..].copy_from_slice(&two_patches[(6 * SIDE * 3) as usize..]);
    for (expected, what) in [
        (&two_patches, "two distant patches"),
        (&bottom_only, "the first patch reverted"),
        (&rgb, "both patches reverted"),
    ] {
        let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &raw_from_rgb(expected));
        client.attach(&toplevel, buffer, SIDE, SIDE);
        pump_until_drawn(&mut pty, &mut terminal, &mut child, expected, what);
        assert_eq!(
            terminal.whole_frames, 1,
            "small changes and reversions must retain the whole-image base"
        );
    }

    detach(&mut pty, &mut child);
}

fn detach(pty: &mut Pty, child: &mut PtyChild) {
    pty.master.write_all(b"\x1bw").unwrap();
    assert!(
        wait_for(Duration::from_secs(10), || child
            .try_wait()
            .unwrap()
            .is_some()),
        "pane did not exit after Alt+W"
    );
    let tail = pty.read_now();
    let text = String::from_utf8_lossy(&tail);
    assert!(
        text.contains("\x1b[?1049l"),
        "the alternate screen was not left: {text:?}"
    );
    assert!(
        text.contains("meowland: detached"),
        "the pane did not say why it left: {text:?}"
    );
}

#[test]
fn edge_mouse_reports_reach_the_client_without_closing_the_pane() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let window = client.create_toplevel("edge clicks", "meowland.test");
    let rgb = gradient();
    let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &raw_from_rgb(&rgb));
    client.attach(&window, buffer, SIDE, SIDE);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let seat = client.seat();
    let pointer = client.get_pointer(seat);
    let mut pty = Pty::open(4, 4, (2, 2));
    let mut child = pty.spawn(
        Command::new(BINARY)
            .args(["attach", "1"])
            .env("XDG_RUNTIME_DIR", &server.runtime),
    );
    let mut terminal = FakeTerminal::new(SIDE as usize, SIDE as usize, (2, 2));
    pump_until_drawn(&mut pty, &mut terminal, &mut child, &rgb, "the first frame");

    for (x, y) in [(3u16, 4u16), (0, 3), (3, 0), (0, 0), (1, 3)] {
        write!(pty.master, "\x1b[<0;{x};{y}M").unwrap();
        let motion = client
            .read_until(|message| message.object == pointer && matches!(message.opcode, 0 | 2));
        let offset = if motion.opcode == 0 { 2 } else { 1 };
        assert_eq!(
            (motion.u32_at(offset), motion.u32_at(offset + 1)),
            (
                u32::from(x.saturating_sub(1)) << 8,
                u32::from(y.saturating_sub(1)) << 8
            ),
            "mouse report ({x}, {y}) reached the wrong pixel"
        );
        let press = client.read_until(|message| message.object == pointer && message.opcode == 3);
        assert_eq!((press.u32_at(2), press.u32_at(3)), (0x110, 1));
        write!(pty.master, "\x1b[<0;{x};{y}m").unwrap();
        let release = client.read_until(|message| message.object == pointer && message.opcode == 3);
        assert_eq!((release.u32_at(2), release.u32_at(3)), (0x110, 0));
        assert!(
            child.try_wait().unwrap().is_none(),
            "edge click closed the pane"
        );
    }
    assert_eq!(server.list().len(), 1);
    detach(&mut pty, &mut child);
}

/// A pane killed from outside hands the terminal back: raw mode, the
/// alternate screen and mouse reporting all outlive a default-action death,
/// and the shell that comes after it has no way to undo them.
#[test]
fn a_signalled_pane_restores_the_terminal() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let toplevel = client.create_toplevel("signal test", "meowland.test");
    let rgb = gradient();
    let (buffer, _file) = client.shm_buffer_with_file(SIDE, SIDE, SIDE * 4, &raw_from_rgb(&rgb));
    client.attach(&toplevel, buffer, SIDE, SIDE);
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "window was never announced"
    );

    let mut pty = Pty::open(4, 4, (2, 2));
    let mut child = pty.spawn(
        Command::new(BINARY)
            .args(["attach", "1"])
            .env("XDG_RUNTIME_DIR", &server.runtime),
    );
    let mut terminal = FakeTerminal::new(SIDE as usize, SIDE as usize, (2, 2));
    pump_until_drawn(&mut pty, &mut terminal, &mut child, &rgb, "the first frame");

    child.signal(rustix::process::Signal::TERM);
    assert!(
        wait_for(Duration::from_secs(10), || child
            .try_wait()
            .unwrap()
            .is_some()),
        "pane did not exit after SIGTERM"
    );
    let text = String::from_utf8_lossy(&pty.read_now()).into_owned();
    assert!(
        text.contains("\x1b[?1049l"),
        "the alternate screen was not left: {text:?}"
    );
    assert!(
        text.contains("\x1b[?1003l"),
        "mouse reporting was left on: {text:?}"
    );
}

#[test]
fn partial_server_packets_do_not_prevent_pane_shutdown() {
    for (packet, signal) in [
        (&[1][..], true),
        (&[16, 0, 0, 0, 0][..], true),
        (&[1][..], false),
    ] {
        let runtime = temp_dir("meowland-partial-packet");
        let listener = UnixListener::bind(runtime.join("meowland-pane.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut pty = Pty::open(4, 4, (2, 2));
        let mut child = pty.spawn(
            Command::new(BINARY)
                .args(["attach", "1"])
                .env("XDG_RUNTIME_DIR", &runtime),
        );
        let mut terminal = FakeTerminal::new(8, 8, (2, 2));
        let mut connection = None;
        let mut hello = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let replies = terminal.feed(&pty.read_now());
            pty.master.write_all(&replies).unwrap();
            if connection.is_none() {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket.set_nonblocking(true).unwrap();
                        connection = Some(socket);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("accepting pane: {error}"),
                }
            }
            if let Some(socket) = &mut connection {
                let mut bytes = [0; 256];
                match socket.read(&mut bytes) {
                    Ok(count) => hello.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("reading pane hello: {error}"),
                }
            }
            if hello.len() >= 4 {
                let size = u32::from_le_bytes(hello[..4].try_into().unwrap()) as usize;
                if hello.len() >= size + 4 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "pane hello was incomplete");
            thread::sleep(Duration::from_millis(2));
        }
        let mut socket = connection.unwrap();
        socket.set_nonblocking(false).unwrap();
        if signal {
            let mut messages = Vec::new();
            let rgb = gradient();
            meowland::protocol::send(&mut messages, &meowland::protocol::ServerToPane::HelloOk)
                .unwrap();
            meowland::protocol::send(
                &mut messages,
                &meowland::protocol::ServerToPane::Frame {
                    width: SIDE,
                    height: SIDE,
                    y: 0,
                    rgb: rgb.clone(),
                },
            )
            .unwrap();
            for byte in messages {
                socket.write_all(&[byte]).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            pump_until_drawn(
                &mut pty,
                &mut terminal,
                &mut child,
                &rgb,
                "a fragmented frame",
            );
        }
        socket.write_all(packet).unwrap();
        if signal {
            thread::sleep(Duration::from_millis(100));
            child.signal(rustix::process::Signal::TERM);
        }
        assert!(
            wait_for(Duration::from_secs(2), || child
                .try_wait()
                .unwrap()
                .is_some()),
            "partial packet blocked termination or the handshake deadline"
        );
        let text = String::from_utf8_lossy(&pty.read_now()).into_owned();
        assert!(
            text.contains("\x1b[?1049l"),
            "terminal was not restored: {text:?}"
        );
        assert!(!signal || child.try_wait().unwrap().unwrap().success());
        std::fs::remove_dir_all(runtime).unwrap();
    }
}
