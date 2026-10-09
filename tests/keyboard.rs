//! Real Kitty key transitions, modifier state, and keyboard teardown.
mod support;

use std::{io::Write as _, process::Command, thread, time::Duration};

use meowland::protocol::{Input, PaneToServer, Show, modifiers};
use support::{
    BINARY, Client, Message, Pane, Pty, PtyChild, Server, fake::FakeTerminal, hello, wait_for,
};

fn window(server: &Server) -> (Client, u32) {
    let mut client = Client::connect(server);
    let window = client.create_toplevel("keyboard", "meowland.test");
    let buffer = client.shm_buffer(8, 8, 32, &[0xff; 256]);
    client.attach(&window, buffer, 8, 8);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let seat = client.seat();
    let keyboard = client.get_keyboard(seat);
    client.sync();
    (client, keyboard)
}

fn attach(server: &Server, native_clipboard: bool) -> (Pty, PtyChild, FakeTerminal) {
    let mut pty = Pty::open(4, 4, (2, 2));
    let mut child = pty.spawn(
        Command::new(BINARY)
            .args(["attach", "1"])
            .env("XDG_RUNTIME_DIR", &server.runtime)
            .env(
                "MEOWLAND_CLIPBOARD",
                if native_clipboard {
                    "native"
                } else {
                    "terminal"
                },
            )
            // A missing host clipboard exercises native paste fallback.
            .env("WAYLAND_DISPLAY", server.runtime.join("missing-wayland"))
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_TTY"),
    );
    let mut terminal = FakeTerminal::new(8, 8, (2, 2));
    assert!(wait_for(Duration::from_secs(5), || {
        let replies = terminal.feed(&pty.read_now());
        pty.master.write_all(&replies).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "pane exited during handshake"
        );
        terminal.whole_frames > 0
    }));
    assert_eq!(terminal.keyboard_flags, 11);
    (pty, child, terminal)
}

fn key(client: &mut Client, keyboard: u32, code: u32, pressed: bool) {
    let message = client.read_until(|message| message.object == keyboard && message.opcode == 3);
    assert_eq!(
        (message.u32_at(2), message.u32_at(3)),
        (code, u32::from(pressed))
    );
}

fn modifier_state(client: &mut Client, keyboard: u32) -> Message {
    client.read_until(|message| message.object == keyboard && message.opcode == 4)
}

fn no_keys(client: &mut Client, keyboard: u32) {
    thread::sleep(Duration::from_millis(75));
    assert!(
        client
            .sync()
            .iter()
            .all(|message| message.object != keyboard || message.opcode != 3),
        "an unreported key transition was synthesized"
    );
}

#[test]
fn kitty_keys_stay_held_until_release_or_terminal_focus_loss() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let (mut pty, mut child, mut terminal) = attach(&server, false);
    client.sync();
    pty.master.write_all(b"\x1b[97;1:1u").unwrap();
    key(&mut client, keyboard, 30, true);
    pty.master.write_all(b"\x1b[97;1:2u\x1b[97;1:2u").unwrap();
    no_keys(&mut client, keyboard);
    pty.master.write_all(b"\x1b[97;1:3u").unwrap();
    key(&mut client, keyboard, 30, false);
    pty.master.write_all(b"\x1b[98;1:1u\x1b[O").unwrap();
    key(&mut client, keyboard, 48, true);
    key(&mut client, keyboard, 48, false);
    // A delayed release after focus loss must not reactivate the old source.
    pty.master.write_all(b"\x1b[98;1:3u").unwrap();
    no_keys(&mut client, keyboard);

    child.signal(rustix::process::Signal::TERM);
    assert!(wait_for(Duration::from_secs(2), || child
        .try_wait()
        .unwrap()
        .is_some()));
    terminal.feed(&pty.read_now());
    assert_eq!(terminal.keyboard_flags, 0);
    assert!(!terminal.modes.contains("1004h"));
}

#[test]
fn standalone_left_and_right_modifiers_locks_and_keypad_reach_wayland() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let (mut pty, _child, _terminal) = attach(&server, false);
    client.sync();

    pty.master.write_all(b"\x1b[57442;5:1u").unwrap();
    assert_eq!(
        modifier_state(&mut client, keyboard).u32_at(1),
        4,
        "Control depressed"
    );
    key(&mut client, keyboard, 29, true);
    pty.master.write_all(b"\x1b[57448;5:1u").unwrap();
    key(&mut client, keyboard, 97, true);
    pty.master.write_all(b"\x1b[57442;5:3u").unwrap();
    key(&mut client, keyboard, 29, false);
    pty.master.write_all(b"\x1b[97;5:1u\x1b[97;5:3u").unwrap();
    key(&mut client, keyboard, 30, true);
    key(&mut client, keyboard, 30, false);
    pty.master.write_all(b"\x1b[57448;1:3u").unwrap();
    assert_eq!(
        modifier_state(&mut client, keyboard).u32_at(1),
        0,
        "Control released"
    );
    key(&mut client, keyboard, 97, false);

    // Caps Lock can already be on when attachment starts; it isn't Shift.
    pty.master.write_all(b"\x1b[97;65:1u\x1b[97;65:3u").unwrap();
    let caps = modifier_state(&mut client, keyboard);
    assert_eq!(caps.u32_at(1), 0);
    assert_eq!(caps.u32_at(3), 2, "Caps Lock locked");
    key(&mut client, keyboard, 30, true);
    key(&mut client, keyboard, 30, false);
    pty.master
        .write_all(b"\x1b[57414;65:1u\x1b[57414;65:3u")
        .unwrap();
    key(&mut client, keyboard, 96, true);
    key(&mut client, keyboard, 96, false);
    pty.master.write_all(b"\x1b[O").unwrap();
    assert_eq!(
        modifier_state(&mut client, keyboard).u32_at(3),
        0,
        "focus loss cleared locks"
    );
}

#[test]
fn adding_alt_after_a_key_press_does_not_intercept_its_release() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let (mut pty, _child, _terminal) = attach(&server, false);
    client.sync();
    pty.master.write_all(b"\x1b[113;1:1u").unwrap();
    key(&mut client, keyboard, 16, true);
    pty.master.write_all(b"\x1b[57443;3:1u").unwrap();
    key(&mut client, keyboard, 56, true);
    // This release is Alt+Q, but its press wasn't a compositor shortcut.
    pty.master.write_all(b"\x1b[113;3:3u").unwrap();
    key(&mut client, keyboard, 16, false);
    pty.master.write_all(b"\x1b[57443;1:3u").unwrap();
    key(&mut client, keyboard, 56, false);
}

#[test]
fn clipboard_fallback_tap_does_not_release_a_real_held_modifier() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let (mut pty, _child, _terminal) = attach(&server, true);
    client.sync();
    pty.master.write_all(b"\x1b[57442;5:1u").unwrap();
    assert_eq!(modifier_state(&mut client, keyboard).u32_at(1), 4);
    key(&mut client, keyboard, 29, true);
    pty.master.write_all(b"\x1b[118;5:1u\x1b[118;1:3u").unwrap();
    key(&mut client, keyboard, 47, true);
    key(&mut client, keyboard, 47, false);
    no_keys(&mut client, keyboard);
    pty.master.write_all(b"\x1b[57442;1:3u").unwrap();
    assert_eq!(modifier_state(&mut client, keyboard).u32_at(1), 0);
    key(&mut client, keyboard, 29, false);
}

#[test]
fn pane_switch_and_disconnect_release_held_keys_without_late_release_interference() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let mut first = Pane::attach(&server, hello(8, 8, Show::Newest));
    let _ = first.frame();
    let mut second = Pane::attach(&server, hello(8, 8, Show::Newest));
    let _ = second.frame();
    let input = |code, pressed| {
        PaneToServer::Input(Input::Key {
            code,
            pressed,
            modifiers: 0,
        })
    };
    first.send(&input(30, true));
    key(&mut client, keyboard, 30, true);
    second.send(&input(48, true));
    key(&mut client, keyboard, 30, false);
    key(&mut client, keyboard, 48, true);
    first.send(&input(30, false));
    no_keys(&mut client, keyboard);
    drop(second);
    key(&mut client, keyboard, 48, false);
    first.send(&input(30, true));
    key(&mut client, keyboard, 30, true);
    first.send(&PaneToServer::Input(Input::ResetKeyboard));
    key(&mut client, keyboard, 30, false);
}

#[test]
fn explicit_clipboard_taps_restore_snapshot_only_modifiers() {
    let server = Server::start();
    let (mut client, keyboard) = window(&server);
    let mut pane = Pane::attach(&server, hello(8, 8, Show::Newest));
    let _ = pane.frame();
    pane.send(&PaneToServer::Input(Input::Key {
        code: 30,
        pressed: true,
        modifiers: modifiers::SHIFT,
    }));
    assert_eq!(modifier_state(&mut client, keyboard).u32_at(1), 1);
    key(&mut client, keyboard, 30, true);
    pane.send(&PaneToServer::Input(Input::KeyTap {
        code: 47,
        modifiers: modifiers::CONTROL,
    }));
    assert_eq!(modifier_state(&mut client, keyboard).u32_at(1), 4);
    key(&mut client, keyboard, 47, true);
    key(&mut client, keyboard, 47, false);
    assert_eq!(
        modifier_state(&mut client, keyboard).u32_at(1),
        1,
        "held Shift restored"
    );
    pane.send(&PaneToServer::Input(Input::ResetKeyboard));
    key(&mut client, keyboard, 30, false);
    // Source teardown can advertise intermediate modifier state with each
    // release; the final reset clears snapshot-only modifiers as well.
    client.read_until(|message| {
        message.object == keyboard && message.opcode == 4 && message.u32_at(1) == 0
    });
}

#[test]
fn missing_or_partial_keyboard_support_is_rejected_and_modes_are_restored() {
    for supported in [false, true] {
        let server = Server::start();
        let mut pty = Pty::open(4, 4, (2, 2));
        let mut child = pty.spawn(
            Command::new(BINARY)
                .args(["attach", "1"])
                .env("XDG_RUNTIME_DIR", &server.runtime),
        );
        let mut terminal = FakeTerminal::new(8, 8, (2, 2));
        terminal.keyboard_supported = supported;
        assert!(wait_for(Duration::from_secs(5), || {
            let replies = terminal.feed(&pty.read_now());
            let replies = String::from_utf8(replies)
                .unwrap()
                .replace("\x1b[?11u", "\x1b[?1u");
            pty.master.write_all(replies.as_bytes()).unwrap();
            child.try_wait().unwrap().is_some()
        }));
        terminal.feed(&pty.read_now());
        assert!(!child.try_wait().unwrap().unwrap().success());
        assert_eq!(terminal.keyboard_flags, 0);
        assert!(!terminal.modes.contains("1049h"));
        assert!(!terminal.modes.contains("1004h"));
        assert!(
            String::from_utf8_lossy(&terminal.text)
                .contains("terminal must support Kitty keyboard")
        );
    }
}
