use std::{fs, io, path::Path};

use anyhow::{Context, Result};
use meowland_server::Settings;

pub(crate) fn load(path: Option<&Path>) -> Result<Settings> {
    let default_path;
    let explicit = path.is_some();
    let path = if let Some(path) = path {
        path
    } else {
        default_path = dirs::config_dir()
            .context("cannot locate config directory")?
            .join("meowland/config.toml");
        &default_path
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if !explicit && error.kind() == io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading configuration {}", path.display()));
        }
    };
    toml::from_str(&contents).with_context(|| format!("loading configuration {}", path.display()))
}
