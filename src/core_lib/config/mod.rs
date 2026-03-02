use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::core_lib::xdg;

pub fn app_config_file(app_id: &str, filename: &str) -> PathBuf {
    xdg::app_config_dir(app_id).join(filename)
}

pub fn load_toml_with_default<T>(path: &Path) -> io::Result<T>
where
    T: DeserializeOwned + Default,
{
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(err) => return Err(err),
    };

    toml::from_str::<T>(&content).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed parsing config {}: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, PartialEq, Eq, Default)]
    struct DummyConfig {
        value: Option<String>,
    }

    #[test]
    fn missing_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.toml");
        let cfg: DummyConfig = load_toml_with_default(&missing).expect("load missing");
        assert_eq!(cfg, DummyConfig::default());
    }

    #[test]
    fn path_builder_appends_filename() {
        let path = app_config_file("spectra", "config.toml");
        assert!(path.ends_with("spectra/config.toml") || path.ends_with("spectra\\config.toml"));
    }
}
