//! Regression checks for readiness-driven pane output.
mod support;

use std::{
    io::Write as _,
    os::unix::net::UnixListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use meowland::protocol::{self, Input, PaneToServer, ServerToPane};
use support::{BINARY, Pty, fake::FakeTerminal, temp_dir};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "end-to-end stalled-terminal setup and recovery"
)]
fn stalled_terminal_keeps_input_responsive_and_defers_ack() {
    const SIDE: u32 = 512;
    let runtime = temp_dir("meowland-output-readiness");
    let listener = UnixListener::bind(runtime.join("meowland-pane.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut pty = Pty::open(SIDE as u16, SIDE as u16, (1, 1));
    let mut child = pty.spawn(
        Command::new(BINARY)
            .args(["attach", "1"])
            .env("XDG_RUNTIME_DIR", &runtime),
    );
    let mut terminal = FakeTerminal::new(SIDE as usize, SIDE as usize, (1, 1));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut connection = None;
    let (messages, reader) = std::sync::mpsc::channel();
    let mut worker = None;
    let mut socket = loop {
        let replies = terminal.feed(&pty.read_now());
        // Exercise inline graphics, not a tiny shared-memory command.
        let replies = String::from_utf8(replies)
            .unwrap()
            .replace("i=32;OK", "i=32;ENOTSUP");
        pty.master.write_all(replies.as_bytes()).unwrap();
        if connection.is_none() {
            match listener.accept() {
                Ok((socket, _)) => {
                    let mut read = socket.try_clone().unwrap();
                    let messages = messages.clone();
                    worker = Some(thread::spawn(move || {
                        while let Ok(message) = protocol::recv::<PaneToServer>(&mut read) {
                            if messages.send(message).is_err() {
                                break;
                            }
                        }
                    }));
                    connection = Some(socket);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept: {error}"),
            }
        }
        if let Ok(message) = reader.try_recv() {
            assert!(matches!(message, PaneToServer::Hello(_)));
            break connection.take().unwrap();
        }
        assert!(Instant::now() < deadline, "pane handshake stalled");
        thread::sleep(Duration::from_millis(2));
    };
    protocol::send(&mut socket, &ServerToPane::HelloOk).unwrap();
    // Deterministic noise defeats compression and exceeds the pty buffer.
    let mut seed = 1u32;
    let rgb: Vec<u8> = (0..SIDE * SIDE * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    protocol::send(
        &mut socket,
        &ServerToPane::Frame {
            width: SIDE,
            height: SIDE,
            y: 0,
            rgb: rgb.clone(),
        },
    )
    .unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(
        reader.try_recv().is_err(),
        "frame was acknowledged before the terminal drained"
    );
    let started = Instant::now();
    for pressed in [true, false] {
        let kind = if pressed { 1 } else { 3 };
        write!(pty.master, "\x1b[97;1:{kind}u").unwrap();
        let message = reader
            .recv_timeout(Duration::from_millis(750))
            .expect("terminal backpressure blocked keyboard input");
        assert!(
            matches!(message, PaneToServer::Input(Input::Key { code: 30, pressed: value, .. }) if value == pressed),
            "unexpected message: {message:?}"
        );
        pty.master.write_all(b"\x1b[97;1:2u").unwrap();
        assert!(
            reader.recv_timeout(Duration::from_millis(50)).is_err(),
            "input was synthesized or the host repeat was forwarded"
        );
    }
    eprintln!("keyboard while terminal stalled: {:?}", started.elapsed());
    for (report, expected) in [(64, 120), (65, -120)] {
        write!(pty.master, "\x1b[<{report};1;1M").unwrap();
        let message = reader
            .recv_timeout(Duration::from_millis(750))
            .expect("terminal backpressure blocked wheel input");
        assert!(
            matches!(message, PaneToServer::Input(Input::Pointer { scroll, .. }) if scroll == expected),
            "each wheel report must deliver a complete detent: {message:?}"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let replies = terminal.feed(&pty.read_now());
        pty.master.write_all(&replies).unwrap();
        if let Ok(message) = reader.try_recv() {
            assert!(matches!(message, PaneToServer::Ack { drawn: true }));
            break;
        }
        assert!(Instant::now() < deadline, "frame did not resume");
        thread::sleep(Duration::from_millis(1));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while terminal.screen() != rgb {
        terminal.feed(&pty.read_now());
        assert!(
            Instant::now() < deadline,
            "terminal did not decode the complete frame (whole frames: {}, shared: {})",
            terminal.whole_frames,
            terminal.shared_frames
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(terminal.shared_frames, 0);
    protocol::send(&mut socket, &ServerToPane::Release("done".into())).unwrap();
    assert!(support::wait_for(Duration::from_secs(2), || child
        .try_wait()
        .unwrap()
        .is_some()));
    socket.shutdown(std::net::Shutdown::Both).unwrap();
    worker.unwrap().join().unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
