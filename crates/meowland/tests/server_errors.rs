//! Server failures as reported by the `meowland` process.

use std::{
    fs,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
};

/// Bytes available for a socket path in `sockaddr_un`, including the NUL.
const SUN_PATH_LEN: usize = 108;

/// The longest socket the server binds directly inside `XDG_RUNTIME_DIR`.
const CONTROL_SOCKET: &str = "meowland-control.sock";

/// Prefix of the Wayland socket name, which ends in the server's process ID.
const WAYLAND_SOCKET_PREFIX: &str = "wayland-meowland-";

/// The longest runtime directory whose control socket path still fits.
const RUNTIME_DIR_LEN: usize = SUN_PATH_LEN - 1 - "/".len() - CONTROL_SOCKET.len();

/// The smallest server process ID whose Wayland socket path no longer fits.
fn min_server_pid() -> u32 {
    let digits = SUN_PATH_LEN - RUNTIME_DIR_LEN - "/".len() - WAYLAND_SOCKET_PREFIX.len();
    10_u32.pow(u32::try_from(digits - 1).expect("digit count fits u32"))
}

/// A uniquely named directory of exactly [`RUNTIME_DIR_LEN`] bytes, removed on
/// drop.
struct RuntimeDir(PathBuf);

impl RuntimeDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir()
            .join(format!("meowland-{tag}-{}-", std::process::id()))
            .into_os_string()
            .into_string()
            .expect("temporary directory is UTF-8");
        assert!(
            path.len() <= RUNTIME_DIR_LEN,
            "temporary directory {path:?} leaves no room for a {RUNTIME_DIR_LEN}-byte runtime directory"
        );
        path.extend(std::iter::repeat_n('x', RUNTIME_DIR_LEN - path.len()));
        let path = PathBuf::from(path);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create runtime directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spawn_server(runtime: &RuntimeDir) -> Child {
    Command::new(env!("CARGO_BIN_EXE_meowland"))
        .arg("server")
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", runtime.path())
        .env("HOME", runtime.path())
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn meowland server")
}

/// Run the server until it exits, retrying with a larger process ID whenever
/// the kernel hands out one too short to overflow the Wayland socket path.
fn run_server_with_long_pid(runtime: &RuntimeDir) -> (u32, Output) {
    let min_pid = min_server_pid();
    for _ in 0..3 {
        let mut server = spawn_server(runtime);
        let pid = server.id();
        if pid >= min_pid {
            return (pid, server.wait_with_output().expect("wait for server"));
        }
        server.kill().expect("kill short-PID server");
        server.wait().expect("reap short-PID server");
        for _ in pid..min_pid {
            thread::spawn(|| {})
                .join()
                .expect("join PID-consuming thread");
        }
    }
    panic!("could not obtain a server process ID of at least {min_pid}");
}

#[test]
fn wayland_socket_path_overflow_reports_its_cause_once() {
    let runtime = RuntimeDir::new("long-wayland");
    let (pid, output) = run_server_with_long_pid(&runtime);

    let wayland = runtime.path().join(format!("{WAYLAND_SOCKET_PREFIX}{pid}"));
    let cause = UnixListener::bind(&wayland)
        .expect_err("Wayland socket path should overflow sun_path")
        .to_string();
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");

    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert_eq!(
        stderr,
        format!("meowland: starting the Wayland compositor: {cause}\n")
    );
    assert_eq!(stderr.matches(&cause).count(), 1);
}
