//! Terminal capability requirements and cleanup on rejection.
mod support;

use std::{
    io::Write as _,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use support::{BINARY, Pty, Server, fake::FakeTerminal};

#[test]
fn pixel_mouse_reporting_is_required() {
    let server = Server::start();
    for report in ["", "\x1b[?1016;0$y", "\x1b[?1016;4$y"] {
        let mut pty = Pty::open(4, 4, (2, 2));
        let mut child = pty.spawn(
            Command::new(BINARY)
                .args(["attach"])
                .env("XDG_RUNTIME_DIR", &server.runtime),
        );
        let mut terminal = FakeTerminal::new(8, 8, (2, 2));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        loop {
            let bytes = pty.read_now();
            output.extend_from_slice(&bytes);
            let replies = String::from_utf8(terminal.feed(&bytes))
                .unwrap()
                .replace("\x1b[?1016;2$y", report);
            pty.master.write_all(replies.as_bytes()).unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success(), "attached without pixel mouse reporting");
                break;
            }
            assert!(Instant::now() < deadline, "capability rejection stalled");
            thread::sleep(Duration::from_millis(2));
        }
        output.extend_from_slice(&pty.read_now());
        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("SGR pixel mouse reporting"),
            "unexpected error: {output}"
        );
        assert!(output.contains("\x1b[?1049l"), "terminal was not restored");
    }
}
