use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    io::IsTerminal,
    os::unix::{net::UnixStream, process::CommandExt as _},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use usage::{Args, Cli, Subcommands, ValueEnum};

use crate::{
    Error, Result, diag,
    protocol::{self, ControlRequest, ControlResponse, Show, WindowInfo},
    server, terminal,
};

#[derive(Cli)]
#[usage(bin = "meowland", version = env!("CARGO_PKG_VERSION"), completion)]
struct Meowland {
    #[usage(subcommand)]
    command: Commands,
}

#[derive(Subcommands)]
enum Commands {
    /// Start a client and show its window in this terminal.
    Run(RunArgs),
    /// Show a window in this terminal.
    Attach(AttachArgs),
    /// List windows.
    List,
    /// Start or stop the compositor server.
    Server(ServerArgs),
    /// Generate a shell completion script.
    Completions(CompletionArgs),
    InternalServer(InternalServerArgs),
}

#[derive(Args)]
struct RunArgs {
    /// The program and arguments to run.
    #[usage(
        required,
        double_dash = "automatic",
        value_hint = usage::ValueHint::CommandWithArguments
    )]
    command: Vec<OsString>,
}

#[derive(Args)]
struct AttachArgs {
    /// The window to show; omit it to follow the focused window.
    #[usage(complete = window_ids)]
    id: Option<u64>,
}

fn window_ids(
    _partial: &<AttachArgs as usage::spec::CommandArgs>::Partial,
    _context: &usage::complete::CompleteCtx<'_>,
) -> Vec<usage::complete::Candidate<'static>> {
    let Ok(paths) = server::Paths::discover() else {
        return Vec::new();
    };
    let Ok(windows) = list_with_timeout(&paths, Duration::from_millis(100)) else {
        return Vec::new();
    };
    windows
        .into_iter()
        .map(|window| usage::complete::Candidate::new(window.id.to_string()))
        .collect()
}

#[derive(Args)]
struct ServerArgs {
    #[usage(subcommand)]
    command: ServerCommands,
}

#[derive(Subcommands)]
enum ServerCommands {
    /// Start the server if it is not already running.
    Start,
    /// Stop the running server.
    Stop,
}

#[derive(Args)]
struct CompletionArgs {
    /// The shell to generate a completion script for.
    #[usage(value_enum)]
    shell: CompletionShell,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Elvish,
    Zsh,
    Fish,
    #[usage(visible_alias = "nushell")]
    Nu,
    #[usage(name = "powershell", visible_alias = "pwsh")]
    PowerShell,
}

impl From<CompletionShell> for usage::complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Nu => Self::Nu,
            CompletionShell::PowerShell => Self::PowerShell,
        }
    }
}

#[derive(Args)]
#[usage(hide)]
struct InternalServerArgs;

/// Parse the process command line and run the selected meowland command.
///
/// # Errors
/// Returns an error when the selected command cannot be completed.
pub fn start() -> Result<()> {
    let cli = Meowland::parse();
    match cli.command {
        Commands::InternalServer(_) => server::serve(&server::Paths::discover()?),
        Commands::Server(ServerArgs { command }) => {
            let paths = server::Paths::discover()?;
            match command {
                ServerCommands::Start => ensure_server(&paths),
                ServerCommands::Stop => request(&paths, &ControlRequest::Stop).map(|_| ()),
            }
        }
        Commands::List => {
            let paths = server::Paths::discover()?;
            for window in list(&paths)? {
                println!(
                    "{}\t{}\t{}\t{}",
                    window.id,
                    protocol::sanitize(&window.app_id),
                    protocol::sanitize(&window.title),
                    if window.active { "active" } else { "" }
                );
            }
            Ok(())
        }
        Commands::Run(RunArgs { command: client }) => {
            let paths = server::Paths::discover()?;
            ensure_server(&paths)?;
            let before = list(&paths)?;
            request(&paths, &ControlRequest::Run(client))?;
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let show = wait_for_new_window(&paths, &before, Duration::from_secs(10))?
                    .map_or(Show::Newest, Show::Id);
                terminal::attach(&paths.pane, show)
            } else {
                Ok(())
            }
        }
        Commands::Attach(AttachArgs { id }) => {
            let paths = server::Paths::discover()?;
            let show = id.map_or(Show::Focused, Show::Id);
            terminal::attach(&paths.pane, show)
        }
        Commands::Completions(CompletionArgs { shell }) => {
            print!("{}", Meowland::completion_script(shell.into()));
            Ok(())
        }
    }
}

/// The window list, as the server last reported it.  `request` has already
/// turned an error reply into an `Err`.
fn list(paths: &server::Paths) -> Result<Vec<WindowInfo>> {
    list_with_timeout(paths, Duration::from_secs(2))
}

fn list_with_timeout(paths: &server::Paths, timeout: Duration) -> Result<Vec<WindowInfo>> {
    let ControlResponse::Windows(windows) =
        request_with_timeout(paths, &ControlRequest::List, timeout)?
    else {
        return Err(Error::UnexpectedWindowListResponse);
    };
    Ok(windows)
}

fn wait_for_new_window(
    paths: &server::Paths,
    before: &[WindowInfo],
    timeout: Duration,
) -> Result<Option<u64>> {
    let deadline = Instant::now() + timeout;
    let existing: HashSet<_> = before.iter().map(|window| window.id).collect();
    while Instant::now() < deadline {
        let windows = list(paths)?;
        if let Some(window) = windows
            .iter()
            .rev()
            .find(|window| !existing.contains(&window.id))
        {
            return Ok(Some(window.id));
        }
        thread::sleep(Duration::from_millis(40));
    }
    Ok(None)
}

fn request(paths: &server::Paths, value: &ControlRequest) -> Result<ControlResponse> {
    request_with_timeout(paths, value, Duration::from_secs(2))
}

fn request_with_timeout(
    paths: &server::Paths,
    value: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse> {
    let mut socket = std::os::unix::net::UnixStream::connect(&paths.control)
        .map_err(|error| Error::io("server is not running", error))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    protocol::send(&mut socket, value)?;
    let reply: ControlResponse = protocol::recv(&mut socket)?;
    if let ControlResponse::Error(error) = &reply {
        return Err(Error::ServerResponse(error.clone()));
    }
    Ok(reply)
}

/// Launchers one `ensure_server` may start before it gives up and reports the
/// log: enough to step over a server that is on its way out, not enough to
/// hammer a binary that cannot start at all.
const LAUNCH_LIMIT: u32 = 4;

fn ensure_server(paths: &server::Paths) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut launcher: Option<Child> = None;
    let mut started = 0;
    loop {
        if request(paths, &ControlRequest::Ping).is_ok() {
            return Ok(());
        }
        // Another server still holding the socket makes a launcher fail to
        // bind, so a new one is started only while nothing is listening and
        // the last launcher has exited: a server on its way out is one that
        // this has to wait for and then replace.  A launcher that dies for
        // any other reason must not be started over and over either.
        let listening = UnixStream::connect(&paths.control).is_ok();
        let exited = launcher
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(Some(_))));
        if !listening && exited && started < LAUNCH_LIMIT {
            launcher = Some(spawn_server(paths)?);
            started += 1;
        }
        if Instant::now() >= deadline {
            return Err(Error::ServerStartup {
                log: paths.log.clone(),
            });
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Start a detached server, with its output going to the log.
fn spawn_server(paths: &server::Paths) -> Result<Child> {
    let executable = env::current_exe()?;
    let log = diag::open(&paths.log)?;
    let mut command = Command::new(executable);
    command
        .arg("internal-server")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own session, so the server outlives this terminal and has no
    // controlling terminal to be hung up on.  `setsid` is done here rather
    // than by spawning an external program that may not be installed.
    #[expect(
        unsafe_code,
        reason = "`CommandExt::pre_exec` is unsafe by signature; the closure only calls setsid"
    )]
    // SAFETY: `pre_exec` is unsafe because the closure runs in the child
    // between fork and exec, where only async-signal-safe work is allowed.
    // `setsid` is a bare syscall that allocates nothing, and the closure
    // captures nothing.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|error| Error::io("starting detached server", error))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

    use super::*;

    fn parse<'a>(words: &'a [&'a str]) -> std::result::Result<Meowland, usage::Error<'static, 'a>> {
        let argv: Vec<&OsStr> = words.iter().map(OsStr::new).collect();
        Meowland::try_parse_from(&argv)
    }

    #[test]
    fn internal_server_is_a_strict_parser_command() {
        let cli = parse(&["meowland", "internal-server"]).expect("hidden command should parse");
        assert!(matches!(cli.command, Commands::InternalServer(_)));
        assert!(parse(&["meowland", "internal-server", "unexpected"]).is_err());

        let help =
            Meowland::render_help(Meowland::command(), true).expect("root help should exist");
        assert!(help.contains("completions"));
        assert!(!help.contains("internal-server"));
    }

    #[test]
    fn run_requires_a_program_but_accepts_program_flags() {
        assert!(parse(&["meowland", "run"]).is_err());
        let cli = parse(&["meowland", "run", "program", "--flag"])
            .expect("program arguments should remain trailing values");
        let Commands::Run(args) = cli.command else {
            panic!("run should select the run command");
        };
        assert_eq!(
            args.command,
            [OsString::from("program"), OsString::from("--flag")]
        );
    }

    #[test]
    fn completion_shells_are_validated_by_the_parser() {
        let cli = parse(&["meowland", "completions", "pwsh"])
            .expect("the advertised PowerShell alias should parse");
        assert!(matches!(
            cli.command,
            Commands::Completions(CompletionArgs {
                shell: CompletionShell::PowerShell
            })
        ));
        assert!(parse(&["meowland", "completions", "not-a-shell"]).is_err());
    }

    #[test]
    fn run_preserves_non_utf8_arguments() {
        let program = OsStr::from_bytes(b"program\xff");
        let argv = [OsStr::new("meowland"), OsStr::new("run"), program];
        let cli = Meowland::try_parse_from(&argv).expect("OsString arguments should be lossless");
        let Commands::Run(run) = cli.command else {
            panic!("run should select the run command");
        };
        assert_eq!(run.command, [program]);
    }
}
