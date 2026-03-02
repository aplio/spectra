use std::path::PathBuf;

pub fn app_config_dir() -> PathBuf {
    crate::core_lib::xdg::app_config_dir("spectra")
}

pub fn app_data_dir() -> PathBuf {
    crate::core_lib::xdg::app_data_dir("spectra")
}
