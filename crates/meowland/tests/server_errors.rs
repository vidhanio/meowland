//! Server failures as reported by the `meowland` process.

use std::{
    fs,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Bytes available for a socket path in `sockaddr_un`, including the NUL.
const SUN_PATH_LEN: usize = 108;

/// A uniquely named runtime directory too long to hold any socket, removed on
/// drop.
struct RuntimeDir(PathBuf);

impl RuntimeDir {
    fn new(tag: &str) -> Self {
        let mut name = format!("meowland-{tag}-{}-", std::process::id());
        name.extend(std::iter::repeat_n('x', SUN_PATH_LEN));
        let path = std::env::temp_dir().join(name);
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

#[test]
fn socket_path_overflow_reports_its_cause_once() {
    let runtime = RuntimeDir::new("long-runtime");
    let control = runtime.path().join("meowland-control.sock");
    let cause = UnixListener::bind(&control)
        .expect_err("control socket path should overflow sun_path")
        .to_string();

    let output = Command::new(env!("CARGO_BIN_EXE_meowland"))
        .arg("server")
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", runtime.path())
        .env("HOME", runtime.path())
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .expect("run meowland server");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");

    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert_eq!(
        stderr,
        format!("meowland: binding {}: {cause}\n", control.display())
    );
    assert_eq!(stderr.matches(&cause).count(), 1);
}
