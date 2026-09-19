use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::DirBuilderExt,
    os::unix::net::UnixStream,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

fn call(binary: &str, runtime: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary)
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .output()
        .expect("launch meowland")
}

fn wayland_globals(runtime: &Path) -> Vec<String> {
    let socket = fs::read_dir(runtime)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("wayland-meowland-")
                && path.extension().is_none()
        })
        .expect("Wayland socket");
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    for (opcode, id) in [(1u32, 2u32), (0, 3)] {
        stream.write_all(&1u32.to_ne_bytes()).unwrap();
        stream
            .write_all(&((12u32 << 16) | opcode).to_ne_bytes())
            .unwrap();
        stream.write_all(&id.to_ne_bytes()).unwrap();
    }
    let mut globals = Vec::new();
    loop {
        let mut header = [0; 8];
        stream.read_exact(&mut header).unwrap();
        let object = u32::from_ne_bytes(header[..4].try_into().unwrap());
        let word = u32::from_ne_bytes(header[4..].try_into().unwrap());
        let size = (word >> 16) as usize;
        assert!((8..=4096).contains(&size));
        let mut body = vec![0; size - 8];
        stream.read_exact(&mut body).unwrap();
        if object == 3 {
            break;
        }
        if object == 2 && (word as u16) == 0 && body.len() >= 8 {
            let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            if len > 0 && body.len() >= 8 + len {
                globals.push(String::from_utf8_lossy(&body[8..8 + len - 1]).into_owned());
            }
        }
    }
    globals
}

#[test]
fn detached_server_accepts_control_requests_and_stops() {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let runtime =
        std::env::temp_dir().join(format!("meowland-smoke-{}-{unique}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let binary = env!("CARGO_BIN_EXE_meowland");

    let start = call(binary, &runtime, &["server", "start"]);
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let list = call(binary, &runtime, &["list"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(list.stdout.is_empty());
    let globals = wayland_globals(&runtime);
    for name in [
        "wl_compositor",
        "wl_shm",
        "xdg_wm_base",
        "wl_seat",
        "wl_output",
    ] {
        assert!(
            globals.iter().any(|global| global == name),
            "missing {name}: {globals:?}"
        );
    }
    let run = call(binary, &runtime, &["run", "true"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stop = call(binary, &runtime, &["server", "stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(4);
    while runtime.join("meowland-control.sock").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !runtime.join("meowland-control.sock").exists(),
        "server did not exit"
    );
    fs::remove_dir_all(runtime).unwrap();
}
