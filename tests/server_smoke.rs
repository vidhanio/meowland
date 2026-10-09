//! Process-level checks of the detached server: it stays up with no pane
//! attached, advertises the globals clients need, runs clients and cleans up
//! after itself.

mod support;

use std::{fs, os::unix::net::UnixListener, process::Command, time::Duration};

use support::{BINARY, Client, Server, program_in_path, temp_dir, wait_for};

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
    let duplicate = server.cli(&["server"]);
    assert!(!duplicate.status.success());
    assert!(
        server.list().is_empty(),
        "the original server lost its socket"
    );

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
fn clients_use_the_servers_wayland_display() {
    let server = Server::start();
    let probe = server.runtime.join("display");
    let run = server.cli(&[
        "run",
        "sh",
        "-c",
        &format!(
            "printf '%s\\n%s' \"$WAYLAND_DISPLAY\" \"${{DISPLAY-unset}}\" > {}",
            probe.display()
        ),
    ]);
    assert!(run.status.success());
    assert!(
        wait_for(Duration::from_secs(5), || probe.exists()),
        "client did not run;\nlog:\n{}",
        server.log()
    );
    let socket = server.wayland_socket();
    let display = socket.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        fs::read_to_string(&probe).unwrap(),
        format!("{display}\nunset")
    );
}

#[test]
fn failed_startup_preserves_unowned_socket_paths() {
    let runtime = temp_dir("meowland-socket-ownership");
    for name in ["meowland-control.sock", "meowland-pane.sock"] {
        let path = runtime.join(name);
        fs::write(&path, "not a socket").unwrap();
        let result = Command::new(BINARY)
            .arg("server")
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(!result.status.success(), "startup replaced a regular file");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a socket");
        fs::remove_file(path).unwrap();
    }
    let pane_path = runtime.join("meowland-pane.sock");
    let pane = UnixListener::bind(&pane_path).unwrap();
    let result = Command::new(BINARY)
        .arg("server")
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(std::os::unix::net::UnixStream::connect(&pane_path).is_ok());
    assert!(!runtime.join("meowland-control.sock").exists());
    drop(pane);
    fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn shutdown_kills_helpers_after_their_parent_exits() {
    let Some(setsid) = program_in_path("setsid") else {
        eprintln!("skipping: setsid is not on PATH");
        return;
    };
    let mut server = Server::start();
    let pidfile = server.runtime.join("helper.pid");
    let script = format!(
        "{} sh -c 'trap \"\" HUP TERM; echo $$ > {}; exec sleep 60' & wait",
        setsid.display(),
        pidfile.display()
    );
    assert!(server.cli(&["run", "sh", "-c", &script]).status.success());
    assert!(wait_for(Duration::from_secs(5), || pidfile.exists()));
    let pid = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    let process = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::PidfdFlags::empty(),
    )
    .unwrap();
    server.stop();
    let exited = wait_for(Duration::from_secs(3), || {
        let mut fds = [rustix::event::PollFd::new(
            &process,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .unwrap()
            > 0
    });
    let _ = rustix::process::pidfd_send_signal(&process, rustix::process::Signal::KILL);
    assert!(exited, "the orphaned helper survived shutdown escalation");
}
