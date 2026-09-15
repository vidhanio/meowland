//! What the command line says.
//!
//! Parsing and nothing else: every command comes out as values, and what those
//! values mean is decided in `crate::start`. The one question here that is for
//! the server rather than for the parser is completion - `attach` offers the
//! IDs the running server actually has, which means asking it while the user
//! is still typing.

use std::{ffi::OsString, path::PathBuf};

use crate::{control, dmabuf};

/// A terminal-native Wayland compositor.
#[derive(usage::Cli)]
#[usage(
    bin = "meowland",
    version = env!("CARGO_PKG_VERSION"),
    arg_required_else_help,
    completion,
    after_help = "Alt+Tab cycles the open windows and Alt+Q stops showing this server here.\n\nA server outlives the terminal it is drawn on: `run` starts one and hands it its command, `attach` shows it, and `list` and `quit` reach it from anywhere else."
)]
pub struct Cli {
    #[usage(subcommand)]
    pub action: Action,
}

#[derive(usage::Subcommands)]
pub enum Action {
    /// Run a client in the server, starting and showing one if there is none
    Run(Run),
    /// Show the server here, or one of its windows
    Attach(Attach),
    /// Print the open windows and the IDs `attach` takes
    List(List),
    /// Stop the server and everything running in it
    Quit(Quit),
    /// Be the server: no terminal of its own, reached over its sockets
    Server(Server),
    /// Print a shell completion script
    Completions(Completions),
}

#[derive(usage::Args)]
pub struct Run {
    #[usage(flatten)]
    pub settings: Settings,

    /// Client command and arguments. Without one, only show the server.
    #[usage(
        value_name = "COMMAND",
        value_hint = usage::ValueHint::CommandWithArguments,
        double_dash = "automatic"
    )]
    pub command: Vec<OsString>,
}

/// How a server is set up, by flags that fall back to environment variables.
#[derive(usage::Args)]
#[usage(
    after_help = "Every setting has a command line flag and an environment variable; the flag wins."
)]
pub struct Settings {
    /// How clients are offered GPU buffers
    #[usage(
        long,
        value_enum,
        value_name = "WHEN",
        env = "MEOWLAND_GPU_BUFFERS",
        default = "auto"
    )]
    pub gpu_buffers: dmabuf::Offer,

    /// The node clients are told to render on, instead of the first one a
    /// renderer can be built on
    #[usage(long, env = "MEOWLAND_RENDER_NODE", value_name = "PATH")]
    pub render_node: Option<PathBuf>,

    /// Where to write logs, instead of `$XDG_RUNTIME_DIR/meowland.log`
    #[usage(long, env = "MEOWLAND_LOG", value_name = "PATH")]
    pub log: Option<PathBuf>,

    /// A `tracing` filter, as in `meowland=debug`
    #[usage(long, env = "MEOWLAND_LOG_LEVEL", value_name = "FILTER")]
    pub log_level: Option<String>,
}

impl Settings {
    /// The settings as the flags that carry them.
    ///
    /// A server is a process of its own, started by the command that found
    /// there was none, so what was decided here has to be said there: the
    /// server reads the same environment, but a flag is not in it.
    pub fn args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("--gpu-buffers"),
            OsString::from(self.gpu_buffers.as_str()),
        ];
        if let Some(node) = &self.render_node {
            args.push(OsString::from("--render-node"));
            args.push(node.clone().into());
        }
        if let Some(path) = &self.log {
            args.push(OsString::from("--log"));
            args.push(path.clone().into());
        }
        if let Some(level) = &self.log_level {
            args.push(OsString::from("--log-level"));
            args.push(level.clone().into());
        }
        args
    }
}

/// The server itself, which is what `run` starts when there is none.
///
/// Not something a person asks for: it is how a server is spelled when the
/// command that needs one starts it. A command here is what the server exists
/// for - it runs the client itself and stops when the client is gone, which is
/// how `meowland run foot` gives the terminal back when foot exits.
#[derive(usage::Args)]
#[usage(hide)]
pub struct Server {
    #[usage(flatten)]
    pub settings: Settings,

    /// Client command and arguments this server is for
    #[usage(
        value_name = "COMMAND",
        value_hint = usage::ValueHint::CommandWithArguments,
        double_dash = "automatic"
    )]
    pub command: Vec<OsString>,
}

#[derive(usage::Args)]
pub struct Attach {
    /// Server-assigned window ID to show, or the window already shown
    #[usage(value_name = "ID", complete = attached_windows)]
    pub window: Option<u64>,
}

#[derive(usage::Args)]
pub struct List;

#[derive(usage::Args)]
pub struct Quit;

#[derive(usage::Args)]
pub struct Completions {
    /// Shell to print the script for
    #[usage(value_enum, value_name = "SHELL")]
    pub shell: CompletionShell,
}

/// The shells a completion script is printed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    #[usage(name = "nu")]
    Nushell,
    #[usage(name = "powershell")]
    PowerShell,
}

/// The script a shell sources to complete meowland's own words.
///
/// The script calls back into `meowland __complete_word__`, so what it offers
/// is answered by this build: the IDs `attach` takes come from the server that
/// is running now (`src/control.rs`).
pub fn script(shell: CompletionShell) -> String {
    Cli::completion_script(shell.into())
}

impl From<CompletionShell> for usage::complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Nushell => Self::Nu,
            CompletionShell::PowerShell => Self::PowerShell,
        }
    }
}

/// The window IDs `attach` may be given, read from the running server.
///
/// A shell asks this while the user is typing, so a server that is not running
/// is an empty answer and not an error: there is nothing to complete, and a
/// complaint printed into someone's half-finished command line would be worse
/// than offering nothing.
fn attached_windows(
    _partial: &<Attach as usage::argv::spec::CommandArgs>::Partial,
    _ctx: &usage::complete::CompleteCtx<'_>,
) -> Vec<usage::complete::Candidate<'static>> {
    let Ok(control::Reply::Windows(windows)) = control::request(&control::Command::List) else {
        return Vec::new();
    };
    windows
        .into_iter()
        .map(|window| {
            let mut description = window.label;
            if window.active {
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str("(active)");
            }
            let id = window.id.to_string();
            if description.is_empty() {
                usage::complete::Candidate::new(id)
            } else {
                usage::complete::Candidate::described(id, description)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        path::PathBuf,
    };

    use super::{Action, Cli, Settings};

    #[test]
    fn settings_are_flags_before_the_command() {
        let parsed = Cli::parse_from(
            [
                "run",
                "--gpu-buffers",
                "off",
                "--render-node",
                "/dev/dri/renderD129",
                "foot",
                "-T",
                "meowland",
            ]
            .map(OsStr::new)
            .as_slice(),
        )
        .expect("settings and a client command should parse");

        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(parsed.settings.gpu_buffers, crate::dmabuf::Offer::Off);
        assert_eq!(
            parsed.settings.render_node,
            Some(PathBuf::from("/dev/dri/renderD129"))
        );
        // The client's own flags are not the compositor's to read.
        assert_eq!(
            parsed.command,
            ["foot", "-T", "meowland"].map(OsString::from)
        );
    }

    #[test]
    fn client_flags_are_forwarded_after_the_command() {
        let parsed = Cli::parse_from(
            ["run", "foot", "--server", "-T", "meowland"]
                .map(OsStr::new)
                .as_slice(),
        )
        .expect("client arguments should parse");

        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(
            parsed.command,
            ["foot", "--server", "-T", "meowland"].map(OsString::from)
        );
    }

    #[test]
    fn a_server_is_started_with_the_settings_that_were_decided() {
        let parsed = Cli::parse_from(
            [
                "run",
                "--gpu-buffers",
                "off",
                "--log-level",
                "meowland=debug",
                "foot",
            ]
            .map(OsStr::new)
            .as_slice(),
        )
        .expect("settings and a client command should parse");
        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };

        let settings: &Settings = &parsed.settings;
        assert_eq!(
            settings.args(),
            ["--gpu-buffers", "off", "--log-level", "meowland=debug"].map(OsString::from),
            "a server of its own is told what this command decided"
        );
    }

    #[test]
    fn a_plain_run_passes_the_defaults_on() {
        let parsed = Cli::parse_from(["run"].map(OsStr::new).as_slice())
            .expect("a run without a command should parse");
        let Action::Run(parsed) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(
            parsed.settings.args(),
            ["--gpu-buffers", "auto"].map(OsString::from)
        );
    }
}
