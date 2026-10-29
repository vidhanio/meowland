use std::{env, path::PathBuf};

use meowland_core::{Error, Result};

/// The server's control and pane socket addresses.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Socket for one-shot control requests.
    pub control: PathBuf,
    /// Socket for persistent terminal-pane connections.
    pub pane: PathBuf,
}

impl Paths {
    /// The sockets inside `XDG_RUNTIME_DIR`.
    ///
    /// # Errors
    /// Returns an error when `XDG_RUNTIME_DIR` is unset or is not a directory.
    pub fn discover() -> Result<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::RuntimeDirectoryUnset)?;
        Self::in_runtime_dir(runtime)
    }

    /// The sockets inside a specific runtime directory.
    ///
    /// # Errors
    /// Returns an error when the directory does not exist.
    fn in_runtime_dir(runtime: impl Into<PathBuf>) -> Result<Self> {
        let runtime = runtime.into();
        if !runtime.is_dir() {
            return Err(Error::RuntimeDirectoryInvalid(runtime));
        }
        Ok(Self {
            control: runtime.join("meowland-control.sock"),
            pane: runtime.join("meowland-pane.sock"),
        })
    }
}
