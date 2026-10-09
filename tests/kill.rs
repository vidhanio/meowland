//! The kill command uses attach's selectors but forcibly disconnects the
//! owning Wayland client, without relying on it to honor a close request.

mod support;

use std::time::Duration;

use meowland::protocol::{ServerToPane, Show};
use support::{Client, Pane, Server, hello, wait_for};

fn mapped(client: &mut Client, title: &str, app_id: &str) {
    let window = client.create_toplevel(title, app_id);
    let buffer = client.shm_buffer(4, 4, 16, &[0xff; 64]);
    client.attach(&window, buffer, 4, 4);
    client.sync();
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

fn expect_windows(server: &Server, titles: &[&str]) {
    assert!(wait_for(Duration::from_secs(5), || {
        let windows = server.list();
        windows.len() == titles.len()
            && titles
                .iter()
                .all(|title| windows.iter().any(|window| window.title == *title))
    }));
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

fn expect_closed(pane: &mut Pane) {
    loop {
        match pane.recv() {
            ServerToPane::Release(reason) => {
                assert_eq!(reason, "window closed");
                return;
            }
            ServerToPane::Title(_) | ServerToPane::Cursor(_) => {}
            other => panic!("expected a release, got {other:?}"),
        }
    }
}

#[test]
fn kill_by_integer_id_removes_all_of_the_clients_windows_and_releases_panes() {
    let server = Server::start();
    let mut target = Client::connect(&server);
    mapped(&mut target, "target", "target.app-id");
    mapped(&mut target, "same client", "another.app-id");
    let mut survivor = Client::connect(&server);
    mapped(&mut survivor, "survivor", "survivor.app-id");
    let id = window_id(&server, "target");
    let second_id = window_id(&server, "same client");
    let mut first_pane = Pane::attach(&server, hello(4, 4, Show::Id(id)));
    let mut second_pane = Pane::attach(&server, hello(4, 4, Show::Id(second_id)));
    first_pane.frame();
    second_pane.frame();

    kill(&server, &["kill", &id.to_string()]);
    expect_closed(&mut first_pane);
    expect_closed(&mut second_pane);
    expect_windows(&server, &["survivor"]);
    survivor.sync();
    let mut pane = Pane::attach(
        &server,
        hello(4, 4, Show::Id(window_id(&server, "survivor"))),
    );
    assert_eq!(pane.frame(), (4, 4, vec![0xff; 48]));
}

#[test]
fn kill_by_app_id_selects_only_the_newest_matching_clients_window() {
    let server = Server::start();
    let mut older = Client::connect(&server);
    mapped(&mut older, "older match", "target.app-id");
    let mut target = Client::connect(&server);
    mapped(&mut target, "newer match", "target.app-id");
    let mut unrelated = Client::connect(&server);
    mapped(&mut unrelated, "unrelated newest", "target.app-id.other");
    expect_windows(&server, &["older match", "newer match", "unrelated newest"]);

    kill(&server, &["kill", "target.app-id"]);
    expect_windows(&server, &["older match", "unrelated newest"]);
    older.sync();
    unrelated.sync();
}

#[test]
fn kill_without_a_target_selects_the_focused_window_not_the_newest() {
    let server = Server::start();
    let mut target = Client::connect(&server);
    mapped(&mut target, "focused target", "target.app-id");
    let mut unrelated = Client::connect(&server);
    mapped(&mut unrelated, "newer window", "other.app-id");
    let id = window_id(&server, "focused target");
    let mut pane = Pane::attach(&server, hello(4, 4, Show::Id(id)));
    pane.frame();
    assert!(wait_for(Duration::from_secs(5), || server
        .list()
        .iter()
        .any(|window| window.id == id && window.active)));

    kill(&server, &["kill"]);
    expect_closed(&mut pane);
    expect_windows(&server, &["newer window"]);
    unrelated.sync();
}

#[test]
fn missing_kill_targets_fail_immediately_without_harming_other_clients() {
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
    let mut survivor = Client::connect(&server);
    mapped(&mut survivor, "survivor", "survivor.app-id");
    let output = server.cli(&["kill", "missing.app-id"]);
    assert!(!output.status.success());
    expect_windows(&server, &["survivor"]);
    survivor.sync();
}
