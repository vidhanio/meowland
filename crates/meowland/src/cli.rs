use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, IsTerminal, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use meowland_core::{Error, Result};
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
    /// Uses `XDG_RUNTIME_DIR` or listeners supplied by socket activation.
    Server(ServerArgs),
    /// Generate a shell completion script.
    Completions(CompletionArgs),
}

#[derive(Args)]
struct ServerArgs {
    /// Read this TOML file instead of `$XDG_CONFIG_HOME/meowland/config.toml`.
    #[usage(long, short = 'c', env = "MEOWLAND_CONFIG")]
    config: Option<PathBuf>,
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
    complete_windows(CompletionTargets::All)
}

fn window_targets(
    _partial: &<WindowArgs as usage_rs::spec::CommandArgs>::Partial,
    _context: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    complete_windows(CompletionTargets::All)
}

fn app_ids(
    _partial: &<RunArgs as usage_rs::spec::CommandArgs>::Partial,
    _context: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    complete_windows(CompletionTargets::AppIds)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionTargets {
    All,
    AppIds,
}

fn complete_windows(targets: CompletionTargets) -> Vec<usage_rs::complete::Candidate<'static>> {
    let Ok(paths) = Paths::discover() else {
        return Vec::new();
    };
    let Ok(mut windows) = list_with_timeout(&paths, Duration::from_millis(100)) else {
        return Vec::new();
    };
    windows.sort_unstable_by_key(|window| std::cmp::Reverse(window.id));
    let include_ids = targets == CompletionTargets::All;
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
pub(crate) fn start() -> Result<()> {
    let Meowland { command } = Meowland::parse();
    crate::init_logging(match &command {
        Commands::Server(_) => crate::LogDestination::Stderr,
        Commands::Run(_) | Commands::Attach(_) => crate::LogDestination::Journal,
        _ => crate::LogDestination::None,
    });
    match command {
        Commands::Server(args) => {
            let paths = Paths::discover()?;
            let settings = crate::configuration::load(args.config.as_deref())?;
            meowland_server::serve(&paths, &settings)
        }
        Commands::List => {
            let paths = Paths::discover()?;
            let mut stdout = io::stdout().lock();
            for window in list(&paths)? {
                writeln!(
                    stdout,
                    "{}\t{}\t{}\t{}",
                    window.id,
                    protocol::sanitize(&window.app_id),
                    protocol::sanitize(&window.title),
                    if window.active { "active" } else { "" }
                )?;
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
                terminal::attach(&paths.pane, show)
            } else {
                Ok(())
            }
        }
        Commands::Attach(AttachArgs { window }) => {
            let paths = Paths::discover()?;
            terminal::attach(&paths.pane, window_target(window))
        }
        Commands::Kill(args) => {
            let paths = Paths::discover()?;
            request(&paths, &ControlRequest::Kill(args.show()))?;
            Ok(())
        }
        Commands::Completions(CompletionArgs { shell }) => {
            write!(
                io::stdout().lock(),
                "{}",
                Meowland::completion_script(shell.into())
            )?;
            Ok(())
        }
    }
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
            "cannot reach the server; run `meowland server` or start `meowland.socket` with systemctl --user",
            error,
        )
    })?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    protocol::send(&mut socket, value)?;
    let reply: ControlResponse = protocol::recv(&mut socket)?;
    match reply {
        ControlResponse::Error(error) => Err(Error::ServerResponse(error)),
        reply => Ok(reply),
    }
}
