//! Process-level checks of the detached server: it stays up with no pane
//! attached, advertises the globals clients need, runs clients and cleans up
//! after itself.

mod support;

use std::time::Duration;

use support::{Client, Server, program_in_path, wait_for, x11_connection_succeeds};

#[test]
fn detached_server_accepts_control_requests_and_stops() {
    let mut server = Server::start();
    let client = Client::connect(&server);
    for global in [
        "wl_compositor",
        "wl_shm",
        "xdg_wm_base",
        "wl_seat",
        "wl_output",
    ] {
        assert!(client.has_global(global), "missing global {global}");
    }
    assert!(server.list().is_empty());

    let run = server.cli(&["run", "true"]);
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    server.stop();
    assert!(!server.runtime.join("meowland-control.sock").exists());
}

#[test]
fn clients_run_without_a_display_when_xwayland_is_off() {
    let server = Server::start();
    let probe = server.runtime.join("display");
    let run = server.cli(&[
        "run",
        "sh",
        "-c",
        &format!("printf %s \"${{DISPLAY-unset}}\" > {}", probe.display()),
    ]);
    assert!(run.status.success());
    assert!(
        wait_for(Duration::from_secs(5), || probe.exists()),
        "client did not run;\nlog:\n{}",
        server.log()
    );
    assert_eq!(std::fs::read_to_string(&probe).unwrap(), "unset");
}

#[test]
fn xwayland_gives_clients_a_working_display() {
    let Some(satellite) = program_in_path("xwayland-satellite") else {
        eprintln!("skipping: xwayland-satellite is not on PATH");
        return;
    };
    let mut server = Server::start_with_env(&[("MEOWLAND_XWAYLAND", satellite.to_str().unwrap())]);
    let probe = server.runtime.join("display");
    let run = server.cli(&[
        "run",
        "sh",
        "-c",
        &format!("printf %s \"${{DISPLAY-unset}}\" > {}", probe.display()),
    ]);
    assert!(run.status.success());
    assert!(
        wait_for(Duration::from_secs(20), || probe.exists()),
        "client did not run;\nlog:\n{}",
        server.log()
    );
    let display = std::fs::read_to_string(&probe).unwrap();
    let number = display
        .strip_prefix(':')
        .unwrap_or_else(|| panic!("DISPLAY was {display:?};\nlog:\n{}", server.log()));
    assert!(number.parse::<u32>().is_ok(), "DISPLAY was {display:?}");
    let socket = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    assert!(
        wait_for(Duration::from_secs(10), || x11_connection_succeeds(&socket)),
        "no X server accepting connections at {};\nlog:\n{}",
        socket.display(),
        server.log()
    );

    server.stop();
    assert!(
        wait_for(Duration::from_secs(10), || !socket.exists()),
        "Xwayland left {} behind;\nlog:\n{}",
        socket.display(),
        server.log()
    );
}

/// A server that has just been asked to stop is still on its way out, and its
/// socket is still open: starting a new one has to wait for that to finish and
/// then try again rather than give up on the first refusal.
#[test]
fn a_server_starts_right_after_one_is_stopped() {
    let server = Server::start();
    let stop = server.cli(&["server", "stop"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let start = server.cli(&["server", "start"]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    // The restarted server answers control requests; `list` panics if it does
    // not.
    let _ = server.list();
}
