use std::io;

/// Errors reported by the command, server, terminal, and renderer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Calloop(#[from] calloop::Error),
    #[error(transparent)]
    WaylandSocket(#[from] smithay::reexports::wayland_server::BindError),
    #[error(transparent)]
    Display(#[from] smithay::reexports::wayland_server::backend::InitError),
    #[error(transparent)]
    Keymap(#[from] smithay::input::keyboard::Error),
    #[error(transparent)]
    Egl(#[from] smithay::backend::egl::Error),
    #[error(transparent)]
    Gles(#[from] smithay::backend::renderer::gles::GlesError),
    #[error(transparent)]
    Sync(#[from] smithay::backend::renderer::sync::Interrupted),
    #[error("the readback is {actual} bytes, not the {expected} it was asked for")]
    ShortReadback { expected: usize, actual: usize },
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDirectory,
    #[error("another meowland server is already running")]
    AlreadyRunning,
    #[error("no meowland server is running")]
    NotRunning,
    #[error("meowland needs a terminal on stdin and stdout (try running it directly)")]
    NotATerminal,
    #[error("this terminal does not support the kitty graphics protocol")]
    NoGraphics,
    #[error("the server sent pixels before it said hello")]
    PixelsFirst,
    #[error("could not watch {source}")]
    Watch {
        source: &'static str,
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("the presenter thread panicked")]
    PresenterPanicked,
    #[error("the server did not come up: {0}")]
    NoServer(String),
    #[error("{0}")]
    Refused(String),
}
