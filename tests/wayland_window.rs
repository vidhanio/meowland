use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

#[path = "../src/protocol.rs"]
mod protocol;

fn msg(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
    let size = 8 + args.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&object.to_ne_bytes());
    out.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    out.extend_from_slice(args);
    out
}
fn u32a(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}
fn string_arg(value: &str) -> Vec<u8> {
    let mut out = (value.len() as u32 + 1).to_ne_bytes().to_vec();
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}
fn read_msg(stream: &mut UnixStream) -> (u32, u16, Vec<u8>) {
    let mut h = [0; 8];
    stream.read_exact(&mut h).unwrap();
    let object = u32::from_ne_bytes(h[..4].try_into().unwrap());
    let word = u32::from_ne_bytes(h[4..].try_into().unwrap());
    let size = (word >> 16) as usize;
    let mut body = vec![0; size - 8];
    stream.read_exact(&mut body).unwrap();
    (object, word as u16, body)
}
fn bind_registry(stream: &mut UnixStream) -> std::collections::HashMap<String, u32> {
    stream.write_all(&msg(1, 1, &u32a(&[2]))).unwrap();
    stream.write_all(&msg(1, 0, &u32a(&[3]))).unwrap();
    let mut globals = std::collections::HashMap::new();
    loop {
        let (object, opcode, body) = read_msg(stream);
        if object == 3 {
            break;
        }
        if object != 2 || opcode != 0 {
            continue;
        }
        let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
        let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
        let end = body[8..].iter().position(|b| *b == 0).unwrap_or(len);
        let interface = String::from_utf8_lossy(&body[8..8 + end]).into_owned();
        globals.insert(interface, name);
    }
    globals
}

#[test]
fn shm_window_reaches_raw_pane() {
    let id = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let runtime = std::env::temp_dir().join(format!("meowland-wayland-{id}"));
    fs::create_dir(&runtime).unwrap();
    let binary = env!("CARGO_BIN_EXE_meowland");
    let start = Command::new(binary)
        .args(["server", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let wayland = (0..100)
        .find_map(|_| {
            let found = fs::read_dir(&runtime)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| p.file_name().is_some_and(|n| { let n = n.to_string_lossy(); n.starts_with("wayland-meowland-") && !n.ends_with(".lock") }));
            if found.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
            found
        })
        .unwrap();
    let mut wl = UnixStream::connect(wayland).unwrap();
    wl.set_nonblocking(false).unwrap();
    wl.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let globals = bind_registry(&mut wl);
    // registry.bind arguments are name, interface, version, new_id.
    for (name, object, version) in [
        ("wl_compositor", 4, 4),
        ("wl_shm", 5, 1),
        ("xdg_wm_base", 6, 1),
    ] {
        let mut args = u32a(&[globals[name], object, version]);
        args.extend_from_slice(&string_arg(name));
        // Correct wire order is name, interface, version, new id.
        args = u32a(&[globals[name]]);
        args.extend_from_slice(&string_arg(name));
        args.extend_from_slice(&u32a(&[version, object]));
        wl.write_all(&msg(2, 0, &args)).unwrap();
    }
    wl.write_all(&msg(4, 0, &u32a(&[7]))).unwrap();
    wl.write_all(&msg(6, 2, &u32a(&[10, 7]))).unwrap();
    wl.write_all(&msg(10, 1, &u32a(&[11]))).unwrap();
    wl.write_all(&msg(7, 6, &[])).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut configured = false;
    while Instant::now() < deadline {
        let (object, opcode, body) = read_msg(&mut wl);
        if object == 10 && opcode == 0 {
            wl.write_all(&msg(10, 4, &body[..4])).unwrap();
            configured = true;
            break;
        }
    }
    assert!(configured);
    let shm_path = runtime.join("buffer");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&shm_path)
        .unwrap();
    file.set_len(16).unwrap();
    let pixels = [
        0x00, 0x00, 0xff, 0xff, 0x00, 0xff, 0x00, 0xff, 0xff, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff,
        0xff,
    ];
    (&file).write_all(&pixels).unwrap();
    let args = u32a(&[8, 16]);
    let mut control = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut control);
    let fds = [file.as_fd()];
    ancillary.push(rustix::net::SendAncillaryMessage::ScmRights(&fds));
    let pool_request = msg(5, 0, &args);
    let iov = [std::io::IoSlice::new(&pool_request)];
    rustix::net::sendmsg(&wl, &iov, &mut ancillary, rustix::net::SendFlags::empty()).unwrap();
    wl.write_all(&msg(8, 0, &u32a(&[0, 2, 2, 8, 1]))).unwrap();
    wl.write_all(&msg(11, 2, &string_arg("test-window")))
        .unwrap();
    wl.write_all(&msg(7, 1, &u32a(&[9, 0, 0]))).unwrap();
    wl.write_all(&msg(7, 2, &u32a(&[0, 0, 2, 2]))).unwrap();
    let mut pane = UnixStream::connect(runtime.join("meowland-pane.sock")).unwrap();
    pane.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    protocol::send(
        &mut pane,
        &protocol::PaneToServer::Hello(protocol::Hello {
            version: protocol::VERSION,
            width: 2,
            height: 2,
            cell_width: Some(1),
            cell_height: Some(1),
            show: protocol::Show::Newest,
        }),
    )
    .unwrap();
    assert!(matches!(
        protocol::recv(&mut pane).unwrap(),
        protocol::ServerToPane::HelloOk
    ));
    let mut control = UnixStream::connect(runtime.join("meowland-control.sock")).unwrap();
    protocol::send(&mut control, &protocol::ControlRequest::List).unwrap();
    let listed = protocol::recv::<protocol::ControlResponse>(&mut control).unwrap();
    assert!(matches!(listed, protocol::ControlResponse::Windows(ref w) if !w.is_empty()));
    assert!(
        matches!(protocol::recv::<protocol::ServerToPane>(&mut pane).unwrap(), protocol::ServerToPane::Frame { width: 2, height: 2, rgb } if rgb == vec![255,0,0,0,255,0,0,0,255,255,255,255])
    );
    let _ = Command::new(binary)
        .args(["server", "stop"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output();
    let _ = fs::remove_dir_all(runtime);
}
