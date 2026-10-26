use std::{
    collections::BTreeMap, ffi::OsString, io::IsTerminal, os::unix::net::UnixStream, time::Duration,
};

use meowland_core::{Error, Result, scale::UiScale};
use meowland_protocol::{
    self as protocol, ControlRequest, ControlResponse, Paths, Show, WindowInfo,
};
use meowland_terminal as terminal;
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
    Attach(AttachArgs),
    /// Ask a window to close.
    Kill(WindowArgs),
    /// List windows.
    List,
    /// Start the compositor server in the foreground.
    ///
    /// Use the systemd service for cleanup of remaining client processes.
    Server(ServerArgs),
    /// Generate a shell completion script.
    Completions(CompletionArgs),
}

#[derive(Args)]
struct ServerArgs {
    /// UI scale for all panes (0.5–4), rounded to the nearest 1/120.
    #[usage(long, default = "1")]
    scale: UiScale,
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
struct AttachArgs {
    /// The integer window ID or app ID to show.
    #[usage(complete = attach_targets)]
    window: String,
}

#[derive(Args)]
struct WindowArgs {
    /// The integer window ID or app ID; omit it to select the focused window.
    #[usage(complete = window_targets)]
    window: Option<String>,
}

impl WindowArgs {
    fn show(self) -> Show {
        self.window.map_or(Show::Focused, window_target)
    }
}

fn window_target(window: String) -> Show {
    window
        .parse()
        .map_or_else(|_| Show::AppId(window), Show::Id)
}

fn attach_targets(
    _partial: &<AttachArgs as usage_rs::spec::CommandArgs>::Partial,
    _context: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    complete_windows(true)
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
    let Ok(paths) = Paths::discover() else {
        return Vec::new();
    };
    let Ok(mut windows) = list_with_timeout(&paths, Duration::from_millis(100)) else {
        return Vec::new();
    };
    windows.sort_unstable_by_key(|window| std::cmp::Reverse(window.id));
    let mut targets = BTreeMap::new();
    for window in windows {
        let title = protocol::sanitize(&window.title);
        if include_ids {
            targets.insert(window.id.to_string(), title.clone());
        }
        if !window.app_id.is_empty() {
            targets.entry(window.app_id).or_insert(title);
        }
    }
    targets
        .into_iter()
        .map(|(value, title)| usage_rs::complete::Candidate::described(value, title))
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
    let Meowland { command } = Meowland::parse();
    crate::init_logging(match &command {
        Commands::Server(_) => crate::LogDestination::Stderr,
        Commands::Run(_) | Commands::Attach(_) => crate::LogDestination::Journal,
        _ => crate::LogDestination::None,
    });
    match command {
        Commands::Server(args) => {
            let paths = Paths::discover()?;
            run_async(meowland_server::serve_scaled(&paths, args.scale))
        }
        Commands::List => {
            let paths = Paths::discover()?;
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
            let paths = Paths::discover()?;
            let launch = ControlRequest::Run {
                args: client,
                env: std::env::vars_os().collect(),
                cwd: std::env::current_dir()
                    .map_err(|error| Error::io("reading launch working directory", error))?,
            };
            let ControlResponse::Started(token) = request(&paths, &launch)? else {
                return Err(Error::UnexpectedRunResponse);
            };
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let show = app_id.map_or(Show::Activation(token), Show::AppId);
                run_async(terminal::attach(&paths.pane, show))
            } else {
                Ok(())
            }
        }
        Commands::Attach(AttachArgs { window }) => {
            let paths = Paths::discover()?;
            run_async(terminal::attach(&paths.pane, window_target(window)))
        }
        Commands::Kill(args) => {
            let paths = Paths::discover()?;
            request(&paths, &ControlRequest::Kill(args.show()))?;
            Ok(())
        }
        Commands::Completions(CompletionArgs { shell }) => {
            print!("{}", Meowland::completion_script(shell.into()));
            Ok(())
        }
    }
}

fn run_async(operation: impl Future<Output = Result<()>>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(operation)
}

fn list(paths: &Paths) -> Result<Vec<WindowInfo>> {
    list_with_timeout(paths, protocol::REQUEST_TIMEOUT)
}

fn list_with_timeout(paths: &Paths, timeout: Duration) -> Result<Vec<WindowInfo>> {
    let ControlResponse::Windows(windows) =
        request_with_timeout(paths, &ControlRequest::List, timeout)?
    else {
        return Err(Error::UnexpectedWindowListResponse);
    };
    Ok(windows)
}

fn request(paths: &Paths, value: &ControlRequest) -> Result<ControlResponse> {
    request_with_timeout(paths, value, protocol::REQUEST_TIMEOUT)
}

fn request_with_timeout(
    paths: &Paths,
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
