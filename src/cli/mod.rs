use std::{ffi::OsString, io::IsTerminal, os::unix::net::UnixStream, time::Duration};

use meowland::{
    Error, Result,
    protocol::{self, ControlRequest, ControlResponse, Show, WindowInfo},
    server, terminal,
};
use usage_rs::{Args, Cli, Subcommands, ValueEnum};

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
    Attach(WindowArgs),
    /// Ask a window to close.
    Kill(WindowArgs),
    /// List windows.
    List,
    /// Start the compositor server in the foreground.
    Server,
    /// Generate a shell completion script.
    Completions(CompletionArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Attach by app ID for clients without xdg-activation-v1 support.
    #[usage(long, complete = app_ids)]
    app_id: Option<String>,
    /// The program and arguments to run.
    #[usage(
        required,
        double_dash = "automatic",
        value_hint = usage_rs::ValueHint::CommandWithArguments
    )]
    command: Vec<OsString>,
}

#[derive(Args)]
struct WindowArgs {
    /// The integer window ID or app ID; omit it to select the focused window.
    #[usage(complete = window_targets)]
    window: Option<String>,
}

impl WindowArgs {
    fn show(self) -> Show {
        self.window.map_or(Show::Focused, |window| {
            window
                .parse()
                .map_or_else(|_| Show::AppId(window), Show::Id)
        })
    }
}

fn window_targets(
    _partial: &<WindowArgs as usage_rs::spec::CommandArgs>::Partial,
    _context: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    complete_windows(true)
}

fn app_ids(
    _partial: &<RunArgs as usage_rs::spec::CommandArgs>::Partial,
    _context: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    complete_windows(false)
}

fn complete_windows(include_ids: bool) -> Vec<usage_rs::complete::Candidate<'static>> {
    let Ok(paths) = server::Paths::discover() else {
        return Vec::new();
    };
    let Ok(windows) = list_with_timeout(&paths, Duration::from_millis(100)) else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for window in windows {
        if include_ids {
            targets.push(window.id.to_string());
        }
        if !window.app_id.is_empty() {
            targets.push(window.app_id);
        }
    }
    targets.sort_unstable();
    targets.dedup();
    targets
        .into_iter()
        .map(usage_rs::complete::Candidate::new)
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

impl From<CompletionShell> for usage_rs::complete::Shell {
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
        Commands::Run(RunArgs {
            app_id,
            command: client,
        }) => {
            let paths = server::Paths::discover()?;
            let ControlResponse::Started(token) = request(&paths, &ControlRequest::Run(client))?
            else {
                return Err(Error::UnexpectedRunResponse);
            };
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let show = app_id.map_or(Show::Activation(token), Show::AppId);
                terminal::attach(&paths.pane, show)
            } else {
                Ok(())
            }
        }
        Commands::Attach(args) => {
            let paths = server::Paths::discover()?;
            terminal::attach(&paths.pane, args.show())
        }
        Commands::Kill(args) => {
            let paths = server::Paths::discover()?;
            request(&paths, &ControlRequest::Kill(args.show()))?;
            Ok(())
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
