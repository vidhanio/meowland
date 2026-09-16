//! meowland: a Wayland compositor that runs inside your terminal.
//!
//! Clients connect to a Wayland socket like they always do. Their windows are
//! rendered into a frame buffer the compositor owns, that frame buffer is
//! diffed against the previous frame, and the differences are sent to the
//! terminal as [kitty graphics protocol][spec] images, tiled into the cell
//! grid. Keyboard and mouse input goes the other way.
//!
//! # A server and its panes
//!
//! A *server* owns the compositor, the windows and the clients it started, and
//! has no terminal of its own: it is reached over two sockets in
//! `$XDG_RUNTIME_DIR`, and it outlives every terminal that shows it. A *pane*
//! is one attached terminal, and it asks to be shown **one window**: the server
//! draws that window into that terminal's own geometry and sends it there.
//! Panes are independent - two of them can be showing the same window, or one
//! each - so nobody is displaced by anybody, and a pane leaving, or being
//! closed, is nothing to the others (`src/display.rs`, `src/client.rs`,
//! `src/server.rs`, and the per-pane state in `src/compositor.rs`).
//!
//! What that leaves the command line is four ways to reach a server: `run`
//! starts one if there is none, hands it a client command and shows that
//! client's window here - the one it opens, which is what `run` waits for
//! before taking the terminal over - `attach` shows one window of it here,
//! `list` prints the windows it has, and `quit` stops it. A server started for
//! a command stops with it, so `meowland run foot` gives the terminal back when
//! foot exits; one that was already running was started by something still
//! using it and stays. `attach` completes the window IDs of the server that is
//! running (`src/cli.rs`, `src/control.rs`).
//!
//! Every window has an ID the server gave it, and a pane shows one of them
//! until it goes: `Alt+Q` asks the window it is showing to close, and a client
//! that takes that request ends its pane with it, which is how the terminal
//! comes back. With nothing shown, `Alt+Q` lets go of the terminal rather than
//! closing anything. The server and its other windows stay either way.
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
/// Long enough for a compositor to build a renderer and bind its sockets, and
/// short enough that a server that cannot start is reported instead of waited
/// for.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Do what the command line says.
pub fn start() -> anyhow::Result<()> {
    match Cli::parse().action {
        Action::Run(run) => run_client(run),
        // An ID is that window; without one, whatever the server has the
        // keyboard on, so that this terminal starts out looking at what is
        // being looked at.
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

/// `meowland run`: run a client in the server, starting and showing one if
/// there is none.
///
/// The command is the server's rather than this process's: a server owns the
/// clients started in it, which is what leaves `attach` something to attach to
/// and what lets a window outlive the terminal it was opened from. This
/// terminal is shown the server too, unless another one already is, in which
/// case the window appears there and this one says so.
fn run_client(run: cli::Run) -> anyhow::Result<()> {
    let cli::Run { settings, command } = run;
    // What the command starts is a window that was not there before it, which
    // is how this knows which one to show: an app that opens its window after a
    // splash screen, or one that opens a second window later, is still shown by
    // the window it announced itself with.
    let before = newest_window().unwrap_or(None);
    give_command(&settings, &command)?;
    // A server started for a command stops with it, and a command that exits at
    // once takes the server with it before this terminal has finished looking
    // at the terminal it was typed in. There is then nothing to show.
    if !server_running() {
        return Ok(());
    }
    // Showing a client takes a terminal to draw on; running it does not. A
    // `run` from a script, or with its output redirected, gives the server its
    // command and leaves it to it.
    if !tty::is_terminal() {
        return Ok(());
    }
    if command.is_empty() {
        return client::attach(display::Show::Newest);
    }
    // The client's window is the one it opens, and this waits for it rather
    // than taking the terminal over for a screen that has nothing on it yet.
    let show = match appeared(before, WINDOW_WAIT) {
        Window::Appeared(id) => display::Show::Window(id),
        // Nothing of its own: the server is shown as it is, and follows
        // whatever the command opens later.
        Window::None => display::Show::Newest,
        // The command is over and so is the server it ran in.
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

/// How long a command is given to open a window before this stops waiting.
///
/// Long enough for an app that has to start up first, and short enough that a
/// command which opens nothing does not leave the terminal sitting there.
const WINDOW_WAIT: Duration = Duration::from_secs(10);

/// How often a command's window is looked for.
const WINDOW_POLL: Duration = Duration::from_millis(50);

/// The newest window the server has, or `None` when it has none yet.
///
/// A server that is not there is an error rather than an empty answer: "no
/// windows" and "no server" are different things to be waiting on.
fn newest_window() -> Result<Option<u64>, control::Error> {
    match control::request(&control::Command::List)? {
        control::Reply::Windows(windows) => Ok(windows.into_iter().map(|window| window.id).max()),
        // The one command that is answered with windows is this one.
        control::Reply::Ok | control::Reply::Failed(_) => Ok(None),
    }
}

/// Wait for a window newer than `before` to appear.
fn appeared(before: Option<u64>, within: Duration) -> Window {
    let deadline = Instant::now() + within;
    loop {
        match newest_window() {
            // A server that is not there is a command that has finished, and
            // nothing of its own appeared - which is a command that ran and
            // left nothing to show rather than a failure.
            Err(error) => {
                tracing::debug!(%error, "could not ask the server for its windows");
                return Window::ServerGone;
            }
            Ok(Some(newest)) if Some(newest) != before => return Window::Appeared(newest),
            // Nothing yet: a server that is up with no windows is one a client
            // is still starting in.
            Ok(None | Some(_)) => {}
        }
        if Instant::now() >= deadline {
            return Window::None;
        }
        thread::sleep(WINDOW_POLL);
    }
}

/// Make sure a server is running the command, starting one for it if need be.
///
/// A server that is already there was started by something that is still using
/// it, so it is told the command and outlives it. One started here exists for
/// the command and stops when the clients it started are gone.
fn give_command(settings: &cli::Settings, command: &[OsString]) -> anyhow::Result<()> {
    if command.is_empty() {
        // Nothing to hand over: this is only a server to show.
        return if server_running() {
            Ok(())
        } else {
            start_server(settings, command)
        };
    }
    if server_running() {
        match control::request(&control::Command::Run(command.to_vec())) {
            Ok(reply) => return accepted(reply),
            // The server went away between the look and the command. A command
            // with nowhere to run gets a server of its own, and one that will
            // not start is reported by that server.
            Err(error) => tracing::debug!(%error, "no server to hand the command to"),
        }
    }
    start_server(settings, command)
}

/// Whether a server is there to be talked to.
///
/// A look cannot fail: every reason a connection did not happen is a reason
/// there is nothing to talk to, and a command that is actually sent reports
/// what went wrong when it is.
fn server_running() -> bool {
    control::connect(control::CONTROL_SOCKET).is_ok()
}

/// Start a server of its own, with nothing of this terminal in it.
///
/// A server is a process of its own on purpose: it outlives the terminal it is
/// shown on, and it is what every later `attach`, `list` and `quit` reaches.
/// The command, if there is one, is the server's to run: it stops when the
/// clients it started are gone, which is how the terminal comes back when the
/// command it was started for exits.
fn start_server(settings: &cli::Settings, command: &[OsString]) -> anyhow::Result<()> {
    let program = std::env::current_exe().context("could not find the meowland binary")?;
    let mut child = Command::new(program)
        .arg("server")
        .args(settings.args())
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Not this terminal's child: closing the terminal must not take the
        // server, or the clients running in it, down with the terminal. It gets
        // a process group of its own for the same reason, since a terminal that
        // is closed signals the group in front of it.
        .process_group(0)
        // Read while it comes up, so that a server that cannot start - a
        // command that is not there, a machine with no renderer - says why
        // here, rather than in a log nobody opens. Everything it says
        // afterwards goes to that log: a terminal being drawn on is no place
        // for it.
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start a server")?;

    // Wait for it rather than guess: a `run` that overtook its own server
    // would start a second one. A server that is still there and not listening
    // is one that is still building a renderer and binding its sockets.
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    let started = loop {
        if let Some(status) = child.try_wait().context("could not wait for the server")? {
            // A server that stopped cleanly is one whose command is already
            // over: it was started for that command, and it stops with it. The
            // caller treats that as nothing to show rather than a failure.
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
        // Only `list` is answered with windows, and this is not `list`.
        control::Reply::Windows(_) => anyhow::bail!("the server answered with a window list"),
    }
}

/// `meowland list`: the open windows, which are what `attach` takes.
fn list_windows() -> anyhow::Result<()> {
    let control::Reply::Windows(windows) = control::request(&control::Command::List)? else {
        anyhow::bail!("the server did not answer with a window list");
    };
    for window in windows {
        let mut fields = vec![window.id.to_string()];
        // The label names the app, the title says what it is doing: one is what
        // a shell would have started it by, the other is what the window itself
        // says, and they are worth telling apart when both are there.
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
