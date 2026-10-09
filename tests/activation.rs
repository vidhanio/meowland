//! Launch selection is driven by xdg-activation-v1 or an explicit app ID,
//! never by whichever window happened to appear next.

mod support;

use std::{fs, time::Duration};

use meowland::protocol::{ControlRequest, ControlResponse, PaneToServer, ServerToPane, Show};
use support::{Client, Pane, Server, Window, hello, wait_for};

const SIDE: u32 = 4;

fn launch(server: &Server) -> String {
    match server.control(&ControlRequest::Run(vec!["true".into()])) {
        ControlResponse::Started(token) => token,
        other => panic!("launch failed: {other:?}"),
    }
}

fn paint(client: &mut Client, window: &Window, color: [u8; 3]) {
    let raw = [color[2], color[1], color[0], 0xff].repeat((SIDE * SIDE) as usize);
    let buffer = client.shm_buffer(SIDE, SIDE, SIDE * 4, &raw);
    client.attach(window, buffer, SIDE, SIDE);
    client.sync();
}

fn mapped(client: &mut Client, title: &str, app_id: &str, color: [u8; 3]) -> Window {
    let window = client.create_toplevel(title, app_id);
    paint(client, &window, color);
    window
}

fn expect_frame(pane: &mut Pane, color: [u8; 3]) {
    assert_eq!(
        pane.frame(),
        (SIDE, SIDE, color.repeat((SIDE * SIDE) as usize))
    );
}

fn expect_release(pane: &mut Pane, reason: &str) {
    loop {
        match pane.recv() {
            ServerToPane::Release(actual) => {
                assert_eq!(actual, reason);
                return;
            }
            ServerToPane::Title(_) | ServerToPane::Cursor(_) => {}
            other => panic!("expected release, got {other:?}"),
        }
    }
}

#[test]
fn launched_clients_receive_unique_tokens_not_the_servers_inherited_token() {
    let server = Server::start_with_env(&[("XDG_ACTIVATION_TOKEN", "inherited-token")]);
    let mut tokens = Vec::new();
    for index in 0..2 {
        let path = server.runtime.join(format!("token-{index}"));
        let script = format!("printf '%s' \"$XDG_ACTIVATION_TOKEN\" > {}", path.display());
        let response = server.control(&ControlRequest::Run(vec![
            "sh".into(),
            "-c".into(),
            script.into(),
        ]));
        let ControlResponse::Started(token) = response else {
            panic!("unexpected launch reply: {response:?}");
        };
        assert_ne!(token, "");
        assert_ne!(token, "inherited-token");
        assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(
            &path
        )
        .is_ok_and(|written| written == token)));
        tokens.push(token);
    }
    assert_ne!(tokens[0], tokens[1]);
    assert!(server.list().is_empty());
    assert!(
        !server
            .cli(&["run", "/nonexistent/meowland-test-client"])
            .status
            .success()
    );
}

#[test]
fn activation_before_mapping_and_pane_connection_is_remembered_and_one_shot() {
    let server = Server::start();
    let token = launch(&server);
    let mut client = Client::connect(&server);
    assert!(client.has_global("xdg_activation_v1"));
    let target = client.create_toplevel("target", "shared.app-id");
    client.activate(&token, target.surface);
    client.sync();
    paint(&mut client, &target, [1, 2, 3]);
    let unrelated = mapped(&mut client, "newer", "shared.app-id", [4, 5, 6]);
    // Redeeming the same token again must not change its recorded target.
    client.activate(&token, unrelated.surface);
    client.sync();

    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Activation(token.clone())));
    expect_frame(&mut pane, [1, 2, 3]);
    let mut duplicate = Pane::attach(&server, hello(SIDE, SIDE, Show::Activation(token)));
    expect_release(&mut duplicate, "no such activation");

    client.destroy_toplevel(&target);
    expect_release(&mut pane, "window closed");
}

#[test]
fn a_pending_launch_ignores_other_windows_and_invalid_activation_tokens() {
    let server = Server::start();
    let token = launch(&server);
    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Activation(token.clone())));
    let mut client = Client::connect(&server);
    let unrelated = mapped(&mut client, "unrelated", "same.app-id", [4, 5, 6]);
    client.activate("not-a-valid-token", unrelated.surface);
    client.sync();
    let target = mapped(&mut client, "target", "same.app-id", [1, 2, 3]);
    client.activate(&token, target.surface);
    client.sync();
    expect_frame(&mut pane, [1, 2, 3]);

    // Once selected, a launch pane stays on its integer window ID.
    mapped(&mut client, "newest", "same.app-id", [7, 8, 9]);
    pane.send(&PaneToServer::Ack { drawn: true });
    paint(&mut client, &target, [10, 11, 12]);
    expect_frame(&mut pane, [10, 11, 12]);
}

#[test]
fn activation_with_a_waiting_pane_still_requires_pixels() {
    let server = Server::start();
    let token = launch(&server);
    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Activation(token.clone())));
    let mut client = Client::connect(&server);
    let target = client.create_toplevel("target", "target.app-id");
    client.activate(&token, target.surface);
    client.sync();
    mapped(&mut client, "unrelated", "other.app-id", [4, 5, 6]);
    paint(&mut client, &target, [1, 2, 3]);
    expect_frame(&mut pane, [1, 2, 3]);
}

#[test]
fn closing_an_activated_window_before_mapping_releases_its_waiting_pane() {
    let server = Server::start();
    let token = launch(&server);
    let mut pane = Pane::attach(&server, hello(SIDE, SIDE, Show::Activation(token.clone())));
    let mut client = Client::connect(&server);
    let target = client.create_toplevel("never mapped", "target.app-id");
    client.activate(&token, target.surface);
    client.sync();
    client.destroy_toplevel(&target);
    expect_release(&mut pane, "window closed");
}

#[test]
fn app_id_selection_waits_for_an_exact_match_including_late_metadata() {
    let server = Server::start();
    let mut pane = Pane::attach(
        &server,
        hello(SIDE, SIDE, Show::AppId("target.app-id".into())),
    );
    let mut client = Client::connect(&server);
    mapped(&mut client, "similar ID", "target.app-id.other", [4, 5, 6]);
    let target = mapped(&mut client, "target", "initial.app-id", [1, 2, 3]);
    client.set_app_id(target.xdg_toplevel, "target.app-id");
    client.sync();
    expect_frame(&mut pane, [1, 2, 3]);

    mapped(&mut client, "same ID, newer", "target.app-id", [7, 8, 9]);
    pane.send(&PaneToServer::Ack { drawn: true });
    paint(&mut client, &target, [10, 11, 12]);
    expect_frame(&mut pane, [10, 11, 12]);
    client.destroy_toplevel(&target);
    expect_release(&mut pane, "window closed");
}

#[test]
fn app_id_selection_waits_for_mapping_and_integer_ids_still_work() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    let target = client.create_toplevel("target", "target.app-id");
    let mut pane = Pane::attach(
        &server,
        hello(SIDE, SIDE, Show::AppId("target.app-id".into())),
    );
    mapped(&mut client, "unrelated", "other.app-id", [4, 5, 6]);
    paint(&mut client, &target, [1, 2, 3]);
    expect_frame(&mut pane, [1, 2, 3]);

    assert!(wait_for(Duration::from_secs(5), || server.list().len() == 2));
    let id = server
        .list()
        .iter()
        .find(|window| window.title == "target")
        .unwrap()
        .id;
    let mut by_id = Pane::attach(&server, hello(SIDE, SIDE, Show::Id(id)));
    expect_frame(&mut by_id, [1, 2, 3]);
    let mut missing = Pane::attach(&server, hello(SIDE, SIDE, Show::Id(u64::MAX)));
    expect_release(&mut missing, "no such window");
}

#[test]
fn app_id_selection_chooses_the_newest_matching_window_not_the_newest_overall() {
    let server = Server::start();
    let mut client = Client::connect(&server);
    mapped(&mut client, "older match", "target.app-id", [4, 5, 6]);
    mapped(&mut client, "newer match", "target.app-id", [1, 2, 3]);
    mapped(&mut client, "unrelated", "other.app-id", [7, 8, 9]);
    let mut pane = Pane::attach(
        &server,
        hello(SIDE, SIDE, Show::AppId("target.app-id".into())),
    );
    expect_frame(&mut pane, [1, 2, 3]);
}
