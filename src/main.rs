use std::{
    env,
    io::IsTerminal,
    os::unix::process::CommandExt as _,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use meowland::{
    protocol::{self, ControlRequest, ControlResponse, Show, WindowInfo},
    server, terminal,
};
use usage::{Args, Cli, Subcommands};

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
}

#[derive(Args)]
struct RunArgs {
    #[usage(double_dash = "automatic", value_hint = usage::ValueHint::CommandWithArguments)]
    command: Vec<String>,
}

#[derive(Args)]
struct AttachArgs {
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
    let Ok(windows) = list(&paths) else {
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
    Start,
    Stop,
}

#[derive(Args)]
struct CompletionArgs {
    shell: String,
}

fn main() {
    if let Err(error) = start() {
        eprintln!("meowland: {error:#}");
        std::process::exit(1);
    }
}

fn start() -> Result<()> {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-server")) {
        return server::serve(&server::Paths::discover()?);
    }
    let cli = Meowland::parse();
    match cli.command {
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
            let before = list(&paths).unwrap_or_default();
            let has_client = !client.is_empty();
            if !client.is_empty() {
                request(&paths, &ControlRequest::Run(client))?;
            }
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let show = if has_client {
                    wait_for_new_window(&paths, &before, Duration::from_secs(10))?
                        .map_or(Show::Newest, Show::Id)
                } else {
                    Show::Newest
                };
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
        Commands::Completions(CompletionArgs { shell }) => completions(&shell),
    }
}

/// The window list, as the server last reported it.  `request` has already
/// turned an error reply into an `Err`.
fn list(paths: &server::Paths) -> Result<Vec<WindowInfo>> {
    let ControlResponse::Windows(windows) = request(paths, &ControlRequest::List)? else {
        bail!("server did not return windows");
    };
    Ok(windows)
}

fn wait_for_new_window(
    paths: &server::Paths,
    before: &[WindowInfo],
    timeout: Duration,
) -> Result<Option<u64>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let windows = list(paths)?;
        if let Some(window) = windows
            .iter()
            .rev()
            .find(|window| !before.iter().any(|old| old.id == window.id))
        {
            return Ok(Some(window.id));
        }
        thread::sleep(Duration::from_millis(40));
    }
    Ok(None)
}

fn request(paths: &server::Paths, value: &ControlRequest) -> Result<ControlResponse> {
    let mut socket =
        std::os::unix::net::UnixStream::connect(&paths.control).context("server is not running")?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    protocol::send(&mut socket, value)?;
    let reply: ControlResponse = protocol::recv(&mut socket)?;
    if let ControlResponse::Error(error) = &reply {
        bail!("{error}");
    }
    Ok(reply)
}

fn ensure_server(paths: &server::Paths) -> Result<()> {
    if request(paths, &ControlRequest::Ping).is_ok() {
        return Ok(());
    }
    let executable = env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)?;
    let mut command = Command::new(executable);
    command
        .arg("--internal-server")
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
    let child = command.spawn().context("starting detached server")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if request(paths, &ControlRequest::Ping).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "server did not become ready (launcher PID {}); see {}",
        child.id(),
        paths.log.display()
    )
}

fn completions(shell: &str) -> Result<()> {
    let shell = usage::complete::Shell::from_name(shell).context("unknown completion shell")?;
    print!("{}", Meowland::completion_script(shell));
    Ok(())
}
