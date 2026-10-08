use std::{
    collections::HashSet,
    ffi::OsString,
    io::IsTerminal,
    os::unix::net::UnixStream,
    thread,
    time::{Duration, Instant},
};

use meowland::{
    Error, Result,
    protocol::{self, ControlRequest, ControlResponse, Show, WindowInfo},
    server, terminal,
};
use usage::{Args, Cli, Subcommands, ValueEnum};

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
    /// Start the compositor server in the foreground.
    Server,
    /// Generate a shell completion script.
    Completions(CompletionArgs),
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

/// Parse the process command line and run the selected meowland command.
///
/// # Errors
/// Returns an error when the selected command cannot be completed.
pub fn start() -> Result<()> {
    let cli = Meowland::parse();
    match cli.command {
        Commands::Server => server::serve(&server::Paths::discover()?),
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
    let mut socket = UnixStream::connect(&paths.control).map_err(|error| {
        Error::io(
            "server is not running; start it with `meowland server`",
            error,
        )
    })?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    protocol::send(&mut socket, value)?;
    let reply: ControlResponse = protocol::recv(&mut socket)?;
    if let ControlResponse::Error(error) = &reply {
        return Err(Error::ServerResponse(error.clone()));
    }
    Ok(reply)
}
