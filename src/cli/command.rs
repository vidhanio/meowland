//! What each command does in a process that has no server of its own: it asks
//! the server to do it, or starts one to ask.

use std::{
    ffi::OsString,
    io::Read as _,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    Error,
    cli::Settings,
    client, logging,
    protocol::{
        WindowId,
        control::{self, Argument, Reply},
        pane::Show,
    },
    server::process,
};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `run` waits for the window of the client it started to appear.
const WINDOW_WAIT: Duration = Duration::from_secs(10);

const WINDOW_POLL: Duration = Duration::from_millis(50);

/// Start a client, wait for its window, and show it here.
pub(super) fn run(run: crate::cli::Run) -> Result<(), Error> {
    let crate::cli::Run { settings, command } = run;
    let before = newest_window().unwrap_or(None);
    give_command(&settings, &command)?;
    if !client::terminal::is_terminal() {
        return Ok(());
    }
    if command.is_empty() {
        return client::attach(Show::Newest);
    }
    let show = match appeared(before, WINDOW_WAIT) {
        Window::Appeared(id) => Show::Window(id),
        Window::None => Show::Newest,
        Window::ServerGone => return Ok(()),
    };
    client::attach(show)
}

enum Window {
    Appeared(WindowId),
    None,
    ServerGone,
}

fn newest_window() -> Result<Option<WindowId>, Error> {
    match control::request(&control::Command::List)? {
        Reply::Windows(windows) => Ok(windows.into_iter().map(|window| window.id).max()),
        Reply::Ok | Reply::Failed(_) => Ok(None),
    }
}

/// The window the server gained while this was waiting, if it gained one.
fn appeared(before: Option<WindowId>, within: Duration) -> Window {
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

fn give_command(settings: &Settings, command: &[OsString]) -> Result<(), Error> {
    if !server_running() {
        start_server(settings)?;
    }
    if command.is_empty() {
        return Ok(());
    }
    let arguments = command.iter().map(Argument::from).collect();
    accepted(control::request(&control::Command::Run(arguments))?)
}

fn server_running() -> bool {
    control::connect(control::CONTROL_SOCKET).is_ok()
}

/// Start a server of this command's own, and wait for it to listen.
pub(super) fn start_server(settings: &Settings) -> Result<(), Error> {
    let program = std::env::current_exe()?;
    let mut command = Command::new(program);
    command
        .args(["server", "start"])
        .args(settings.args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Capture startup errors before logging is initialized.
        .stderr(Stdio::piped());
    process::spawn_detached(&mut command);
    let mut child = command.spawn()?;

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

fn accepted(reply: Reply) -> Result<(), Error> {
    match reply {
        Reply::Ok => Ok(()),
        Reply::Failed(reason) => Err(Error::Refused(reason)),
        Reply::Windows(_) => Err(Error::Refused(
            "the server answered with a window list".to_owned(),
        )),
    }
}

/// Print the windows the server has, one per line, for a terminal to read.
pub(super) fn list() -> Result<(), Error> {
    let Reply::Windows(windows) = control::request(&control::Command::List)? else {
        return Err(Error::Refused(
            "the server did not answer with a window list".to_owned(),
        ));
    };
    for window in windows {
        let mut fields = vec![window.id.to_string()];
        if !window.label.is_empty() {
            fields.push(printable(&window.label));
        }
        if !window.title.is_empty() {
            fields.push(printable(&window.title));
        }
        if window.active {
            fields.push("active".to_owned());
        }
        println!("{}", fields.join("\t"));
    }
    Ok(())
}

/// A name as a terminal may be shown it.
///
/// A client supplies these, so control characters are removed: printed as they
/// are, they would be escapes the terminal acts on.
fn printable(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Stop the server, and every client it started.
pub(super) fn stop() -> Result<(), Error> {
    accepted(control::request(&control::Command::Stop)?)
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn a_window_name_cannot_print_terminal_controls() {
        assert_eq!(printable("app\x1b[2J"), "app [2J");
        assert_eq!(printable("two\nlines\there"), "two lines here");
        assert_eq!(printable("title\u{7f}"), "title ");
        assert_eq!(printable("~ /code"), "~ /code");
    }
}
