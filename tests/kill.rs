//! The kill command uses attach's selectors and sends `xdg_toplevel.close`,
//! leaving the client to close the selected window.

mod support;

use std::time::Duration;

use meowland::protocol::{ServerToPane, Show};
use support::{Client, Pane, Server, Window, hello, wait_for};

fn mapped(client: &mut Client, title: &str, app_id: &str) -> Window {
    let window = client.create_toplevel(title, app_id);
    let buffer = client.shm_buffer(4, 4, 16, &[0xff; 64]);
    client.attach(&window, buffer, 4, 4);
    client.sync();
    window
}

fn window_id(server: &Server, title: &str) -> u64 {
    assert!(wait_for(Duration::from_secs(5), || server
        .list()
        .iter()
        .any(|window| window.title == title)));
    server
        .list()
        .into_iter()
        .find(|window| window.title == title)
        .unwrap()
        .id
}

fn kill(server: &Server, args: &[&str]) {
    let output = server.cli(args);
    assert!(
        output.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, []);
}

#[test]
fn kill_by_integer_id_requests_only_that_window_to_close() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let target = mapped(&mut client, "target", "target.app-id");
    let other = mapped(&mut client, "same client", "another.app-id");
    let id = window_id(&server, "target");
    let mut pane = Pane::attach(&server, hello(4, 4, Show::Id(id)));
    pane.frame();

    kill(&server, &["kill", &id.to_string()]);
    let messages = client.sync();
    let close_requests: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.opcode == 1
                && [target.xdg_toplevel, other.xdg_toplevel].contains(&message.object)
        })
        .map(|message| message.object)
        .collect();
    assert_eq!(close_requests, [target.xdg_toplevel]);
    assert_eq!(
        server.list().len(),
        2,
        "a close request must not destroy windows"
    );

    // The pane is released only when the app honors the close request.
    client.destroy_toplevel(&target);
    loop {
        match pane.recv() {
            ServerToPane::Release(reason) => {
                assert_eq!(reason, "window closed");
                break;
            }
            ServerToPane::Title(_) | ServerToPane::Cursor(_) => {}
            other => panic!("expected a release, got {other:?}"),
        }
    }
    client.sync();
    assert!(wait_for(Duration::from_secs(5), || server.list().len() == 1));
    assert_eq!(server.list()[0].title, "same client");
}

#[test]
fn kill_by_app_id_requests_the_newest_matching_window_to_close() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let older = mapped(&mut client, "older match", "target.app-id");
    let target = mapped(&mut client, "newer match", "target.app-id");
    let unrelated = mapped(&mut client, "unrelated newest", "target.app-id.other");
    window_id(&server, "unrelated newest");

    kill(&server, &["kill", "target.app-id"]);
    let messages = client.sync();
    let close_requests: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.opcode == 1
                && [
                    older.xdg_toplevel,
                    target.xdg_toplevel,
                    unrelated.xdg_toplevel,
                ]
                .contains(&message.object)
        })
        .map(|message| message.object)
        .collect();
    assert_eq!(close_requests, [target.xdg_toplevel]);
    assert_eq!(server.list().len(), 3);
}

#[test]
fn kill_without_a_target_requests_the_focused_window_not_the_newest() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let target = mapped(&mut client, "focused target", "target.app-id");
    let newer = mapped(&mut client, "newer window", "other.app-id");
    let id = window_id(&server, "focused target");
    let mut pane = Pane::attach(&server, hello(4, 4, Show::Id(id)));
    pane.frame();
    client.sync();

    kill(&server, &["kill"]);
    let messages = client.sync();
    assert!(
        messages
            .iter()
            .any(|message| message.object == target.xdg_toplevel && message.opcode == 1)
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.object == newer.xdg_toplevel && message.opcode == 1)
    );
    assert_eq!(server.list().len(), 2);
}

#[test]
fn missing_kill_targets_fail_immediately_without_harming_other_windows() {
    let server = Server::start();
    for args in [
        &["kill"][..],
        &["kill", "18446744073709551615"][..],
        &["kill", "missing.app-id"][..],
    ] {
        let output = server.cli(args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("no such window"));
    }
    let mut client = Client::connect(&server);
    let survivor = mapped(&mut client, "survivor", "survivor.app-id");
    let output = server.cli(&["kill", "missing.app-id"]);
    assert!(!output.status.success());
    assert!(
        !client
            .sync()
            .iter()
            .any(|message| message.object == survivor.xdg_toplevel && message.opcode == 1)
    );
    assert_eq!(server.list().len(), 1);
}
