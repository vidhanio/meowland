use std::{fs, io, path::Path};

use meowland_core::{Error, Result};
use meowland_server::Settings;

pub(crate) fn load(path: Option<&Path>) -> Result<Settings> {
    let default_path;
    let explicit = path.is_some();
    let path = if let Some(path) = path {
        path
    } else {
        default_path = dirs::config_dir()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "cannot locate config directory")
            })?
            .join("meowland/config.toml");
        &default_path
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if !explicit && error.kind() == io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => {
            return Err(Error::io(
                format!("reading configuration {}", path.display()),
                error,
            ));
        }
    };
    toml::from_str(&contents).map_err(|error| {
        Error::io(
            format!("loading configuration {}", path.display()),
            io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })
}
