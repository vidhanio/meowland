//! A Wayland compositor rendered through the kitty graphics protocol.

mod buffer;
mod cli;
mod client;
mod compositor;
mod control;
mod display;
mod dmabuf;
mod error;
mod gpu;
mod keys;
mod kitty;
mod logging;
mod presenter;
mod process;
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

pub use error::Error;

use crate::cli::{Action, Cli};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);

pub fn start() -> Result<(), Error> {
    match Cli::parse().action {
        Action::Run(run) => run_client(run),
        Action::Attach(attach) => {
            client::attach(
                attach
                    .window
                    .map_or(display::Show::Focused, display::Show::Window),
            )?;
            Ok(())
        }
        Action::List(_) => list_windows(),
        Action::Server(server) => match server.action {
            cli::ServerAction::Start(start) => {
                server::run(start.settings)?;
                Ok(())
            }
            cli::ServerAction::Stop(_) => stop_server(),
        },
        Action::Completions(completions) => {
            print!("{}", cli::script(completions.shell));
            Ok(())
        }
    }
}

fn run_client(run: cli::Run) -> Result<(), Error> {
    let cli::Run { settings, command } = run;
    let before = newest_window().unwrap_or(None);
    give_command(&settings, &command)?;
    if !tty::is_terminal() {
        return Ok(());
    }
    if command.is_empty() {
        return client::attach(display::Show::Newest);
    }
    let show = match appeared(before, WINDOW_WAIT) {
        Window::Appeared(id) => display::Show::Window(id),
        Window::None => display::Show::Newest,
        Window::ServerGone => return Ok(()),
    };
    client::attach(show)
}

enum Window {
    Appeared(u64),
    None,
    ServerGone,
}

const WINDOW_WAIT: Duration = Duration::from_secs(10);

const WINDOW_POLL: Duration = Duration::from_millis(50);

fn newest_window() -> Result<Option<u64>, Error> {
    match control::request(&control::Command::List)? {
        control::Reply::Windows(windows) => Ok(windows.into_iter().map(|window| window.id).max()),
        control::Reply::Ok | control::Reply::Failed(_) => Ok(None),
    }
}

fn appeared(before: Option<u64>, within: Duration) -> Window {
    let deadline = Instant::now() + within;
    loop {
        match newest_window() {
            Err(error) => {
                tracing::debug!(%error, "could not ask the server for its windows");
                return Window::ServerGone;
            }
            Ok(Some(newest)) if Some(newest) != before => return Window::Appeared(newest),
            Ok(None | Some(_)) => {}
        }
        if Instant::now() >= deadline {
            return Window::None;
        }
        thread::sleep(WINDOW_POLL);
    }
}

fn give_command(settings: &cli::Settings, command: &[OsString]) -> Result<(), Error> {
    if !server_running() {
        start_server(settings)?;
    }
    if command.is_empty() {
        return Ok(());
    }
    accepted(control::request(&control::Command::Run(command.to_vec()))?)
}

fn server_running() -> bool {
    control::connect(control::CONTROL_SOCKET).is_ok()
}

fn start_server(settings: &cli::Settings) -> Result<(), Error> {
    let program = std::env::current_exe()?;
    let mut child = Command::new(program)
        .args(["server", "start"])
        .args(settings.args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep terminal hangups from signaling the server.
        .process_group(0)
        // Capture startup errors before logging is initialized.
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    let started = loop {
        if let Some(status) = child.try_wait()? {
            break Err(format!("it stopped before it was listening ({status})"));
        }
        if server_running() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(format!(
                "it was not listening after {SERVER_START_TIMEOUT:?}: see {}",
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
        return Err(Error::NoServer(if said.is_empty() {
            started
        } else {
            format!("{started}: {said}")
        }));
    }
    Ok(())
}

fn accepted(reply: control::Reply) -> Result<(), Error> {
    match reply {
        control::Reply::Ok => Ok(()),
        control::Reply::Failed(reason) => Err(Error::Refused(reason)),
        control::Reply::Windows(_) => Err(Error::Refused(
            "the server answered with a window list".to_owned(),
        )),
    }
}

fn list_windows() -> Result<(), Error> {
    let control::Reply::Windows(windows) = control::request(&control::Command::List)? else {
        return Err(Error::Refused(
            "the server did not answer with a window list".to_owned(),
        ));
    };
    for window in windows {
        let mut fields = vec![window.id.to_string()];
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

fn stop_server() -> Result<(), Error> {
    accepted(control::request(&control::Command::Stop)?)
}
