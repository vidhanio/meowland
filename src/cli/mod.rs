//! The command line: the flags, the subcommands, and what each one does.

mod command;

use std::{ffi::OsString, path::PathBuf};

use crate::{
    Error, client, dmabuf,
    protocol::{WindowId, control, pane::Show},
    server,
};

/// Do what the command line says.
pub fn execute(action: Action) -> Result<(), Error> {
    match action {
        Action::Run(run) => command::run(run),
        Action::Attach(attach) => client::attach(attach.window.map_or(Show::Focused, Show::Window)),
        Action::List(_) => command::list(),
        Action::Server(server) => match server.action {
            ServerAction::Start(start) => {
                if server::process::is_detached() {
                    server::run(start.settings)
                } else {
                    command::start_server(&start.settings)
                }
            }
            ServerAction::Stop(_) => command::stop(),
        },
        Action::Completions(completions) => {
            print!("{}", script(completions.shell));
            Ok(())
        }
    }
}

/// A Wayland compositor in a terminal.
#[derive(usage::Cli)]
#[usage(
    bin = "meowland",
    version = env!("CARGO_PKG_VERSION"),
    arg_required_else_help,
    completion,
    after_help = "Alt+Q closes the shown window or detaches an empty pane; Alt+W detaches the pane. The server stays running after a pane exits."
)]
pub struct Cli {
    #[usage(subcommand)]
    pub action: Action,
}

#[derive(usage::Subcommands)]
pub enum Action {
    /// Run a client and show its window
    Run(Run),
    /// Show a window in this terminal
    Attach(Attach),
    /// List open windows
    List(List),
    /// Start or stop the server
    Server(Server),
    /// Print shell completions
    Completions(Completions),
}

#[derive(usage::Args)]
pub struct Run {
    #[usage(flatten)]
    pub settings: Settings,

    /// Client command; omit to show the server without starting a client
    #[usage(
        value_name = "COMMAND",
        value_hint = usage::ValueHint::CommandWithArguments,
        double_dash = "automatic"
    )]
    pub command: Vec<OsString>,
}

#[derive(usage::Args)]
pub struct Settings {
    /// GPU buffer offering mode
    #[usage(
        long,
        value_enum,
        value_name = "WHEN",
        env = "MEOWLAND_GPU_BUFFERS",
        default = "auto"
    )]
    pub gpu_buffers: dmabuf::Offer,

    /// Render node to offer clients
    #[usage(long, env = "MEOWLAND_RENDER_NODE", value_name = "PATH")]
    pub render_node: Option<PathBuf>,

    /// Log path
    #[usage(long, env = "MEOWLAND_LOG", value_name = "PATH")]
    pub log: Option<PathBuf>,

    /// Log filter, such as `meowland=debug`
    #[usage(long, env = "MEOWLAND_LOG_LEVEL", value_name = "FILTER")]
    pub log_level: Option<String>,
}

impl Settings {
    /// Pass resolved flags to a new server process.
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

#[derive(usage::Args)]
pub struct Server {
    #[usage(subcommand)]
    pub action: ServerAction,
}

#[derive(usage::Subcommands)]
pub enum ServerAction {
    /// Start a server without attaching a terminal
    Start(Start),
    /// Stop the server and its clients
    Stop(Stop),
}

#[derive(usage::Args)]
pub struct Start {
    #[usage(flatten)]
    pub settings: Settings,
}

#[derive(usage::Args)]
pub struct Stop;

#[derive(usage::Args)]
pub struct Attach {
    /// Window ID; defaults to the focused window
    #[usage(value_name = "ID", complete = attached_windows)]
    pub window: Option<WindowId>,
}

#[derive(usage::Args)]
pub struct List;

#[derive(usage::Args)]
pub struct Completions {
    /// Shell name
    #[usage(value_enum, value_name = "SHELL")]
    pub shell: CompletionShell,
}

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

/// Complete IDs from the running server.
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
            let mut description = if window.title.is_empty() {
                window.label
            } else {
                window.title
            };
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
        os::unix::ffi::OsStrExt as _,
        path::PathBuf,
    };

    use super::{Action, Cli, Settings};

    #[test]
    fn a_command_carries_the_bytes_a_shell_gave_it() {
        // An argument is not text: it is whatever bytes the shell had, and
        // `run` hands them to the client as they are.
        let parsed = Cli::parse_from(
            [
                OsStr::new("run"),
                OsStr::new("program"),
                OsStr::new("plain"),
                OsStr::from_bytes(b"not \xff text"),
            ]
            .as_slice(),
        )
        .expect("settings and a client command should parse");

        let Action::Run(run) = parsed.action else {
            panic!("expected run command");
        };
        assert_eq!(run.command[2].as_bytes(), b"not \xff text".as_slice());
    }

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
