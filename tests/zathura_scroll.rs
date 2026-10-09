//! Optional real GTK 3/Girara check; requires zathura and a PDF plugin on PATH.
mod support;

use std::{
    fs,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use meowland::protocol::{Input, PaneToServer, Show};
use support::{Pane, Server, hello};

struct Application(Child);
impl Drop for Application {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A tall striped page whose pixels visibly change when scrolled.
fn document() -> Vec<u8> {
    use std::fmt::Write as _;
    let mut content = String::new();
    for row in 0..120 {
        let color = f64::from((row * 37) % 101) / 100.0;
        writeln!(content, "{color} 0.3 0.7 rg 0 {} 600 20 re f", row * 20).unwrap();
    }
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 2400] /Resources << >> /Contents 4 0 R >>"
            .to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ];
    let mut pdf = "%PDF-1.4\n".to_owned();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 5\n0000000000 65535 f ").unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .unwrap();
    pdf.into_bytes()
}

#[test]
#[ignore = "requires zathura with a PDF plugin; manual GTK scroll check"]
fn zathura_is_borderless_and_scrolls() {
    let server = Server::start_with_env(&[("MEOWLAND_RENDER_NODE", "off")]);
    let path = server.runtime.join("scroll.pdf");
    fs::write(&path, document()).unwrap();
    fs::write(
        server.runtime.join("zathurarc"),
        "set adjust-open width\nset scroll-step 100\n",
    )
    .unwrap();
    let trace = fs::File::create(server.runtime.join("zathura.trace")).unwrap();
    let _application = Application(
        Command::new("zathura")
            .arg("--config-dir")
            .arg(&server.runtime)
            .arg(&path)
            .env("GDK_BACKEND", "wayland")
            .env("XDG_RUNTIME_DIR", &server.runtime)
            .env("XDG_DATA_HOME", server.runtime.join("data"))
            .env("XDG_CACHE_HOME", server.runtime.join("cache"))
            .env("WAYLAND_DISPLAY", server.wayland_socket())
            .env("WAYLAND_DEBUG", "client")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace)
            .spawn()
            .expect("zathura must be installed for this ignored test"),
    );
    assert!(
        server.wait_for_window(Duration::from_secs(5)),
        "zathura did not map a window"
    );
    let mut pane = Pane::attach(&server, hello(320, 240, Show::Newest));
    let _ = pane.frame();
    // Allow the application to commit the pane's resized document view while
    // the initial frame is still in flight, then consume the coalesced update.
    thread::sleep(Duration::from_secs(1));
    pane.send(&PaneToServer::Ack { drawn: true });
    let (width, _, before) = pane.frame();
    // The PDF fills the document view. A GTK titlebar would put gray chrome
    // here instead of the page's green/blue stripes near the top of the pane.
    let offset = (8 * width as usize + width as usize / 2) * 3;
    let top = &before[offset..offset + 3];
    assert!(
        (75..=79).contains(&top[1]) && (177..=181).contains(&top[2]),
        "document should start at the top, not below a titlebar: {top:?}"
    );
    pane.send(&PaneToServer::Ack { drawn: true });
    thread::sleep(Duration::from_millis(200));
    // Two half-speed reports provide a complete detent for step-based apps.
    for _ in 0..2 {
        pane.send(&PaneToServer::Input(Input::Pointer {
            x: 160.0,
            y: 120.0,
            button: None,
            pressed: true,
            scroll: -120,
        }));
        thread::sleep(Duration::from_millis(20));
    }
    let (_, _, after) = pane.frame();
    assert!(
        before != after,
        "zathura received wheel input but did not scroll"
    );
    let trace = fs::read_to_string(server.runtime.join("zathura.trace")).unwrap();
    assert!(
        trace.contains(".axis("),
        "zathura did not receive wheel input"
    );
    assert!(
        trace.lines().any(
            |line| line.contains("org_kde_kwin_server_decoration_manager")
                && line.contains(".default_mode(2)")
        ),
        "GTK did not receive the server-side decoration default"
    );
    eprintln!("real zathura: no titlebar, and half-speed wheel input changed document pixels");
}
