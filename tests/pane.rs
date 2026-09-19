//! The pane end to end: a real `meowland attach` process on a pty, a
//! simulated kitty terminal on the other end, and pixel comparisons of what
//! the pane draws.

mod support;

use std::{
    io::Write as _,
    os::unix::fs::FileExt as _,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use support::{BINARY, Client, Pty, PtyChild, Server, fake::FakeTerminal, wait_for};

const SIDE: u32 = 4;

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

    let mut pty = Pty::open(2, 2, (2, 2));
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

    // Alt+W detaches; the pane must hand the terminal back.
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

    let mut pty = Pty::open(2, 2, (2, 2));
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
