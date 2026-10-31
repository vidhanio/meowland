//! Headless end-to-end smoke test: an isolated server, a detached
//! `meowland launch`, `meowland focus` in a pane on a pseudo-terminal,
//! verified frames and input, `meowland quit`, and a clean shutdown.
//!
//! Cargo exposes only this package's binaries to its tests, so the workload
//! comes from `CARGO_BIN_EXE_meowland-workload` and [`Binaries::build`]
//! builds `meowland` beside it with the same profile, keeping plain
//! `cargo test` self-contained and never running a stale compositor.

use std::time::Duration;

use meowland_bench::{
    binaries::Binaries,
    endpoint::Transport,
    pattern::{APP_ID, Pattern},
    session::{Attach, Config, STEP_TIMEOUT, Session},
};

#[test]
fn launch_focus_present_and_quit() {
    let binaries =
        Binaries::build(env!("CARGO_BIN_EXE_meowland-workload")).expect("building meowland");
    let mut session = Session::start(
        &binaries,
        Config {
            width: 640,
            height: 360,
            pattern: Pattern::Sparse,
            transport: Transport::Shared,
            attach: Attach::Focus,
        },
    )
    .expect("the focused pane shows the workload's first frame");

    let windows = session.windows().unwrap();
    let [window] = windows.as_slice() else {
        panic!("expected exactly the workload window, got {windows:?}");
    };
    assert_eq!(window.app_id, APP_ID);
    assert!(
        window.active,
        "the focused window is not active: {window:?}"
    );
    let id = window.id.clone();

    session
        .frame(1)
        .expect("a committed frame reaches the pane");
    session
        .input(2)
        .expect("a key press reaches the workload and its response the pane");

    session.quit(&id).unwrap();
    session
        .until("the window to close", STEP_TIMEOUT, |session| {
            session
                .windows()
                .is_ok_and(|windows| windows.iter().all(|window| window.id != id))
        })
        .unwrap();

    let status = session.stop_server(Duration::from_secs(10)).unwrap();
    assert!(
        status.success(),
        "server exited with {status} after SIGTERM"
    );
}
