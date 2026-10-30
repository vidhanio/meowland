use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, IsTerminal, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{
    ArgGroup, Args, CommandFactory, Parser, Subcommand, ValueHint, builder::PossibleValuesParser,
};
use clap_complete::{
    CompleteEnv,
    engine::{
        ArgValueCandidates, ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
    },
    env::Shells,
};
use meowland_protocol::{
    self as protocol, ControlRequest, ControlResponse, Follow, Paths, Selector, Target, WindowInfo,
    WindowState,
};
use meowland_terminal as terminal;

const BIN: &str = "meowland";
const COMPLETE_VAR: &str = "COMPLETE";

#[derive(Parser)]
#[command(name = BIN, version)]
struct Meowland {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Launch a client and show its windows in this terminal.
    Launch(LaunchArgs),
    /// Show a window in this terminal.
    Focus(FocusArgs),
    /// Ask a window to close.
    ///
    /// Without a selector, closes the focused window.
    Quit(QuitArgs),
    /// List windows.
    List,
    /// Start the compositor server in the foreground.
    ///
    /// Uses `XDG_RUNTIME_DIR` or listeners supplied by socket activation.
    Server(ServerArgs),
    /// Generate a shell completion script.
    Completions(CompletionArgs),
}

impl Commands {
    /// Where tracing events from this command should go.
    pub(crate) const fn log_destination(&self) -> crate::logging::Destination {
        match self {
            Self::Server(_) => crate::logging::Destination::Stderr,
            Self::Launch(_) | Self::Focus(_) => crate::logging::Destination::Journal,
            Self::Quit(_) | Self::List | Self::Completions(_) => crate::logging::Destination::None,
        }
    }
}

#[derive(Args)]
pub(crate) struct ServerArgs {
    /// Read this TOML file instead of `$XDG_CONFIG_HOME/meowland/config.toml`.
    #[arg(long, short, env = "MEOWLAND_CONFIG")]
    config: Option<PathBuf>,
}

/// App ID and title selectors; consumers limit them to one.
#[derive(Args)]
#[group(skip)]
pub(crate) struct MatchArgs {
    /// Match windows with exactly this app ID.
    #[arg(long, add = ArgValueCandidates::new(app_id_candidates))]
    app_id: Option<String>,
    /// Match windows with exactly this title.
    #[arg(long, add = ArgValueCandidates::new(title_candidates))]
    title: Option<String>,
}

impl MatchArgs {
    fn into_selector(self) -> Option<Selector> {
        self.app_id
            .map(Selector::AppId)
            .or_else(|| self.title.map(Selector::Title))
    }
}

/// An app ID, title, or unique window ID selector; consumers limit them to
/// one.
#[derive(Args)]
#[group(skip)]
pub(crate) struct WindowArgs {
    #[command(flatten)]
    matching: MatchArgs,
    /// The unique window ID, as printed by `meowland list`.
    #[arg(value_name = "ID", add = ArgValueCandidates::new(id_candidates))]
    id: Option<String>,
}

impl WindowArgs {
    fn into_selector(self) -> Option<Selector> {
        self.id
            .map(Selector::Id)
            .or_else(|| self.matching.into_selector())
    }
}

#[derive(Args)]
#[command(group(ArgGroup::new("selector").args(["app_id", "title"])))]
pub(crate) struct LaunchArgs {
    #[command(flatten)]
    matching: MatchArgs,
    /// Exit when the first shown window closes instead of waiting for the
    /// next matching window.
    #[arg(long)]
    no_follow: bool,
    /// The program and arguments to launch.
    #[arg(
        required = true,
        trailing_var_arg = true,
        num_args = 1..,
        value_name = "COMMAND",
        value_hint = ValueHint::CommandWithArguments,
        add = ArgValueCompleter::new(CommandCompleter)
    )]
    command: Vec<OsString>,
}

#[derive(Args)]
#[command(group(ArgGroup::new("selector").args(["app_id", "title", "id"]).required(true)))]
pub(crate) struct FocusArgs {
    #[command(flatten)]
    window: WindowArgs,
    /// Wait for a matching window, and for the next one whenever the shown
    /// window closes, instead of exiting.
    #[arg(long, conflicts_with = "id")]
    follow: bool,
}

#[derive(Args)]
#[command(group(ArgGroup::new("selector").args(["app_id", "title", "id"])))]
pub(crate) struct QuitArgs {
    #[command(flatten)]
    window: WindowArgs,
}

#[derive(Args)]
pub(crate) struct CompletionArgs {
    /// The shell to generate a completion script for.
    #[arg(value_parser = PossibleValuesParser::new(Shells::builtins().names()))]
    shell: String,
}

/// Completes an executable on `PATH`, or a path once the value contains `/`
/// or is clearly an argument.
#[derive(Clone, Copy)]
struct CommandCompleter;

impl ValueCompleter for CommandCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        self.complete_at(0, current)
    }

    fn complete_at(&self, index: usize, current: &OsStr) -> Vec<CompletionCandidate> {
        let Some(prefix) = current.to_str() else {
            return Vec::new();
        };
        if index > 0 || prefix.contains('/') {
            return PathCompleter::any().complete(current);
        }
        let mut names = BTreeSet::new();
        for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_str().is_some_and(|name| name.starts_with(prefix))
                    && fs::metadata(entry.path()).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                {
                    names.insert(name);
                }
            }
        }
        names.into_iter().map(CompletionCandidate::new).collect()
    }
}

fn id_candidates() -> Vec<CompletionCandidate> {
    candidates(|window| Some((window.id, format!("{}: {}", window.app_id, window.title))))
}

fn app_id_candidates() -> Vec<CompletionCandidate> {
    candidates(|window| (!window.app_id.is_empty()).then_some((window.app_id, window.title)))
}

fn title_candidates() -> Vec<CompletionCandidate> {
    candidates(|window| (!window.title.is_empty()).then_some((window.title, window.app_id)))
}

/// Complete from the running server's windows, newest first per value.
fn candidates(
    describe: impl Fn(WindowInfo) -> Option<(String, String)>,
) -> Vec<CompletionCandidate> {
    let Ok(paths) = Paths::discover() else {
        return Vec::new();
    };
    let Ok(windows) = list_with_timeout(&paths, Duration::from_millis(100)) else {
        return Vec::new();
    };
    let mut values = BTreeMap::new();
    for window in windows.into_iter().rev() {
        let window = WindowInfo {
            id: protocol::sanitize(&window.id),
            app_id: protocol::sanitize(&window.app_id),
            title: protocol::sanitize(&window.title),
            ..window
        };
        if let Some((value, help)) = describe(window) {
            values.entry(value).or_insert(help);
        }
    }
    values
        .into_iter()
        .map(|(value, help)| CompletionCandidate::new(value).help(Some(help.into())))
        .collect()
}

/// Answer shell completion requests, or parse the process command line into
/// the selected meowland command.
pub(crate) fn parse() -> Commands {
    CompleteEnv::with_factory(Meowland::command)
        .var(COMPLETE_VAR)
        .complete();
    Meowland::parse().command
}

/// Run the selected meowland command.
///
/// # Errors
/// Returns an error when the selected command cannot be completed.
pub(crate) fn run(command: Commands) -> Result<()> {
    match command {
        Commands::Server(args) => {
            let paths = Paths::discover()?;
            let settings = crate::configuration::load(args.config.as_deref())?;
            meowland_server::serve(&paths, &settings)?;
            Ok(())
        }
        Commands::List => {
            let paths = Paths::discover()?;
            let mut stdout = io::stdout().lock();
            for window in list(&paths)? {
                writeln!(
                    stdout,
                    "{}\t{}\t{}\t{}",
                    protocol::sanitize(&window.id),
                    protocol::sanitize(&window.app_id),
                    protocol::sanitize(&window.title),
                    match window.state {
                        WindowState::Active => "active",
                        WindowState::Inactive => "",
                    }
                )?;
            }
            Ok(())
        }
        Commands::Launch(LaunchArgs {
            matching,
            no_follow,
            command: client,
        }) => {
            let paths = Paths::discover()?;
            let launch = ControlRequest::Launch {
                args: client,
                env: std::env::vars_os().collect(),
                cwd: std::env::current_dir().context("reading launch working directory")?,
            };
            let ControlResponse::Launched(token) = request(&paths, &launch)? else {
                bail!("server did not return a launch activation token");
            };
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let target = matching
                    .into_selector()
                    .map_or(Target::Activation(token), Target::Selector);
                let follow = if no_follow {
                    Follow::First
                } else {
                    Follow::Every
                };
                terminal::focus(&paths.pane, target, follow)?;
            }
            Ok(())
        }
        Commands::Focus(FocusArgs { window, follow }) => {
            let paths = Paths::discover()?;
            let follow = if follow {
                Follow::Every
            } else {
                Follow::Current
            };
            terminal::focus(
                &paths.pane,
                Target::Selector(window.into_selector().expect("clap requires one selector")),
                follow,
            )?;
            Ok(())
        }
        Commands::Quit(QuitArgs { window }) => {
            let paths = Paths::discover()?;
            request(&paths, &ControlRequest::Quit(window.into_selector()))?;
            Ok(())
        }
        Commands::Completions(CompletionArgs { shell }) => {
            let shells = Shells::builtins();
            let completer = shells
                .completer(&shell)
                .expect("clap restricts the shell to built-in completers");
            completer.write_registration(COMPLETE_VAR, BIN, BIN, BIN, &mut io::stdout().lock())?;
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
        bail!("server did not return windows");
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
    let mut socket = UnixStream::connect(&paths.control).context(
        "cannot reach the server; run `meowland server` or start `meowland.socket` with systemctl --user",
    )?;
    socket
        .set_read_timeout(Some(timeout))
        .context("configuring the control socket")?;
    socket
        .set_write_timeout(Some(timeout))
        .context("configuring the control socket")?;
    protocol::send(&mut socket, value).context("sending the control request")?;
    let reply: ControlResponse =
        protocol::recv(&mut socket).context("reading the control response")?;
    match reply {
        ControlResponse::Error(error) => bail!("server rejected the request: {error}"),
        reply => Ok(reply),
    }
}
