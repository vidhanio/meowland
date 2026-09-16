//! meowland: a Wayland compositor that runs inside a terminal.
//!
//! Clients connect to a Wayland socket as usual. The compositor draws their
//! windows into a frame buffer it owns, diffs that buffer against the previous
//! frame, and sends the differences to the terminal as
//! [kitty graphics protocol][spec] images in the cell grid. Keyboard and mouse
//! input goes the other way.
//!
//! # A server and its panes
//!
//! A *server* owns the compositor, the windows and the clients it started. It
//! has no terminal of its own: it is reached over two sockets in
//! `$XDG_RUNTIME_DIR`, and it outlives every terminal that shows it. A *pane*
//! is one attached terminal. Each pane asks for one window, and the server
//! draws that window into that terminal's geometry and sends it there. Panes
//! are independent: two panes can show the same window, or one each, and no
//! pane displaces another (`src/display.rs`, `src/client.rs`, `src/server.rs`,
//! and the per-pane state in `src/compositor.rs`).
//!
//! The command line has four ways to reach a server. `run` starts one if there
//! is none, hands it a client command, and shows the window that client opens.
//! `attach` shows one window of a running server here. `list` prints the
//! windows. `quit` stops the server. A server that was started for a command
//! stops with it, so `meowland run foot` gives the terminal back when foot
//! exits. A server that was already running was started by something that still
//! uses it, and it stays. `attach` completes the window IDs of the running
//! server (`src/cli.rs`, `src/control.rs`).
//!
//! Each window has an ID that the server gave it, and a pane shows one of them
//! until it goes away. `Alt+Q` asks the window that the pane shows to close,
//! and a client that takes that request ends its pane with it, which gives the
//! terminal back. With nothing shown, `Alt+Q` releases the terminal. The server
//! and its other windows stay in both cases.
//!
//! [spec]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

mod buffer;
mod cli;
mod client;
mod compositor;
mod control;
mod display;
mod dmabuf;
mod gpu;
mod keys;
mod kitty;
mod logging;
mod presenter;
mod render;
mod server;
mod tty;
mod xwayland;

use std::{
    ffi::OsString,
    io::Read as _,
    os::unix::process::CommandExt as _,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context as _;

use crate::cli::{Action, Cli};

/// How long a `run` waits for the server it started to come up.
///
/// Long enough to build a renderer and bind the sockets, and short enough to
/// report a server that cannot start.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Do what the command line says.
pub fn start() -> anyhow::Result<()> {
    match Cli::parse().action {
        Action::Run(run) => run_client(run),
        // An ID selects that window. Without one, the window with the keyboard
        // is used, so this terminal starts on what is being looked at.
        Action::Attach(attach) => client::attach(
            attach
                .window
                .map_or(display::Show::Focused, display::Show::Window),
        ),
        Action::List(_) => list_windows(),
        Action::Quit(_) => quit_server(),
        Action::Server(server) => server::run(server.settings, server.command),
        Action::Completions(completions) => {
            print!("{}", cli::script(completions.shell));
            Ok(())
        }
    }
}

/// `meowland run`: run a client in the server, starting one if there is none,
/// and show its window here.
///
/// The command belongs to the server, not to this process, so a window can
/// outlive the terminal it was opened from. This terminal shows the server too,
/// unless another terminal already does.
fn run_client(run: cli::Run) -> anyhow::Result<()> {
    let cli::Run { settings, command } = run;
    // Show the window that the command opens. Wait for a window that did not
    // exist before the command, so that a splash screen or a second window does
    // not displace the application's own window.
    let before = newest_window().unwrap_or(None);
    give_command(&settings, &command)?;
    // A command that exits at once takes its server with it, before this
    // terminal has finished examining the terminal it was typed in.
    if !server_running() {
        return Ok(());
    }
    // Running a command needs no terminal. A `run` from a script, or with its
    // output redirected, gives the server its command and exits.
    if !tty::is_terminal() {
        return Ok(());
    }
    if command.is_empty() {
        return client::attach(display::Show::Newest);
    }
    let show = match appeared(before, WINDOW_WAIT) {
        Window::Appeared(id) => display::Show::Window(id),
        // Nothing of its own, so follow whatever it opens later.
        Window::None => display::Show::Newest,
        // The command is over, and its server with it.
        Window::ServerGone => return Ok(()),
    };
    client::attach(show)
}

/// What became of the window a command was expected to open.
enum Window {
    Appeared(u64),
    None,
    ServerGone,
}

/// How long a command is given to open a window.
///
/// Long enough for an application to start up, and short enough that a command
/// which opens nothing does not hold the terminal.
const WINDOW_WAIT: Duration = Duration::from_secs(10);

/// How often a command's window is looked for.
const WINDOW_POLL: Duration = Duration::from_millis(50);

/// The newest window the server has, or `None` if it has none yet.
///
/// A missing server is an error, because "no windows" and "no server" are
/// different answers to wait on.
fn newest_window() -> Result<Option<u64>, control::Error> {
    match control::request(&control::Command::List)? {
        control::Reply::Windows(windows) => Ok(windows.into_iter().map(|window| window.id).max()),
        // `list` is the one command answered with windows.
        control::Reply::Ok | control::Reply::Failed(_) => Ok(None),
    }
}

/// Wait for a window newer than `before` to appear.
fn appeared(before: Option<u64>, within: Duration) -> Window {
    let deadline = Instant::now() + within;
    loop {
        match newest_window() {
            // The server has finished its command and left nothing to show.
            Err(error) => {
                tracing::debug!(%error, "could not ask the server for its windows");
                return Window::ServerGone;
            }
            Ok(Some(newest)) if Some(newest) != before => return Window::Appeared(newest),
            // A server that is up with no windows is one that a client is still
            // starting in.
            Ok(None | Some(_)) => {}
        }
        if Instant::now() >= deadline {
            return Window::None;
        }
        thread::sleep(WINDOW_POLL);
    }
}

/// Make sure a server runs the command, and start one for it if necessary.
///
/// A server that is already there was started by something that still uses it,
/// so it is told the command and outlives it. A server started here belongs to
/// the command and stops when the clients it started are gone.
fn give_command(settings: &cli::Settings, command: &[OsString]) -> anyhow::Result<()> {
    if command.is_empty() {
        // Nothing to hand over. This is only a server to show.
        return if server_running() {
            Ok(())
        } else {
            start_server(settings, command)
        };
    }
    if server_running() {
        match control::request(&control::Command::Run(command.to_vec())) {
            Ok(reply) => return accepted(reply),
            // The server went away between the check and the command. That
            // command gets a server of its own, which reports a command that
            // will not start.
            Err(error) => tracing::debug!(%error, "no server to hand the command to"),
        }
    }
    start_server(settings, command)
}

/// Whether a server is there to talk to.
///
/// A failed connection is not an error here. Nothing to talk to is a normal
/// answer, and a command that is sent reports its own failure.
fn server_running() -> bool {
    control::connect(control::CONTROL_SOCKET).is_ok()
}

/// Start a server of its own, with nothing of this terminal in it.
///
/// The server outlives the terminal that shows it, and it is what every later
/// `attach`, `list` and `quit` reaches. The command, if there is one, belongs
/// to the server, which stops when the clients it started are gone.
fn start_server(settings: &cli::Settings, command: &[OsString]) -> anyhow::Result<()> {
    let program = std::env::current_exe().context("could not find the meowland binary")?;
    let mut child = Command::new(program)
        .arg("server")
        .args(settings.args())
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // A process group of its own, so that closing the terminal does not
        // signal the server or the clients in it.
        .process_group(0)
        // Read stderr while the server starts, so that a server which cannot
        // start reports it here instead of in a log. Everything it says later
        // goes to that log, because the terminal it draws on is no place for
        // it.
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start a server")?;

    // Wait for the server instead of guessing, because a `run` that overtook
    // its own server would start a second one.
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    let started = loop {
        if let Some(status) = child.try_wait().context("could not wait for the server")? {
            // A clean stop means the command is already over, so there is
            // nothing to show and this is not a failure.
            break if status.success() {
                Ok(())
            } else {
                Err(format!("the server stopped before it came up ({status})"))
            };
        }
        if server_running() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(format!(
                "the server did not come up: see {}",
                logging::path(settings.log.as_deref()).display()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    if let Err(started) = started {
        let mut said = String::new();
        if let Some(mut errors) = child.stderr.take() {
            let _ = errors.read_to_string(&mut said);
        }
        let said = said.trim();
        if said.is_empty() {
            anyhow::bail!("{started}");
        }
        anyhow::bail!("{started}: {said}");
    }
    Ok(())
}

/// Whether a server carried a command out.
fn accepted(reply: control::Reply) -> anyhow::Result<()> {
    match reply {
        control::Reply::Ok => Ok(()),
        control::Reply::Failed(reason) => anyhow::bail!(reason),
        // `list` is the only command answered with windows, and this is not
        // `list`.
        control::Reply::Windows(_) => anyhow::bail!("the server answered with a window list"),
    }
}

/// `meowland list`: print the open windows.
fn list_windows() -> anyhow::Result<()> {
    let control::Reply::Windows(windows) = control::request(&control::Command::List)? else {
        anyhow::bail!("the server did not answer with a window list");
    };
    for window in windows {
        let mut fields = vec![window.id.to_string()];
        // The label is what a shell started the app by, the title is what the
        // client says. Both are printed when both are there.
        if !window.label.is_empty() {
            fields.push(window.label);
        }
        if !window.title.is_empty() {
            fields.push(window.title);
        }
        if window.active {
            fields.push("active".to_owned());
        }
        println!("{}", fields.join("\t"));
    }
    Ok(())
}

/// `meowland quit`: stop the server and everything running in it.
fn quit_server() -> anyhow::Result<()> {
    accepted(control::request(&control::Command::Quit)?)
}
