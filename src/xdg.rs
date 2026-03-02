use std::env;
use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

pub fn data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
}

pub fn app_config_dir() -> PathBuf {
    config_home().join("spectra")
}

pub fn app_data_dir() -> PathBuf {
    data_home().join("spectra")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths_append_spectra() {
        let config = app_config_dir();
        let data = app_data_dir();
        assert!(config.ends_with("spectra"));
        assert!(data.ends_with("spectra"));
    }
}
