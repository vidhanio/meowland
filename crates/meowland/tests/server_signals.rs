//! Signal-driven shutdown of a running `meowland server` process.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How long the server may take to start or to finish its graceful shutdown.
const DEADLINE: Duration = Duration::from_secs(10);

/// A uniquely named runtime directory, removed on drop.
struct RuntimeDir(PathBuf);

impl RuntimeDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("meowland-{tag}-{}", std::process::id()));
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

fn meowland(runtime: &RuntimeDir, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_meowland"));
    command
        .args(args)
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", runtime.path())
        .env("HOME", runtime.path())
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Wait until the server answers a control request, so its signal handling is
/// installed.
fn wait_until_serving(runtime: &RuntimeDir, server: &mut Child) {
    let deadline = Instant::now() + DEADLINE;
    while !meowland(runtime, &["list"])
        .status()
        .expect("run meowland list")
        .success()
    {
        assert!(
            server.try_wait().expect("poll server").is_none(),
            "server exited during startup"
        );
        assert!(Instant::now() < deadline, "server did not start");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_exit(server: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = server.try_wait().expect("poll server") {
            return status;
        }
        if Instant::now() >= deadline {
            server.kill().expect("kill server");
            server.wait().expect("reap server");
            panic!("server ignored the signal");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hangup_shuts_the_server_down_gracefully() {
    let runtime = RuntimeDir::new("hangup");
    let mut server = meowland(&runtime, &["server"])
        .spawn()
        .expect("spawn meowland server");
    wait_until_serving(&runtime, &mut server);

    let sent = Command::new("kill")
        .args(["-HUP", &server.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let status = wait_for_exit(&mut server);

    assert!(status.success(), "{status}");
    let remaining: Vec<_> = fs::read_dir(runtime.path())
        .expect("read runtime directory")
        .map(|entry| entry.expect("read runtime entry").file_name())
        .collect();
    assert!(remaining.is_empty(), "{remaining:?}");
}
