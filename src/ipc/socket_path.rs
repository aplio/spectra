use std::env;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const APP_ID: &str = "spectra";
const SOCKET_FILE: &str = "spectra.sock";

pub fn socket_path() -> PathBuf {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(APP_ID).join(SOCKET_FILE);
    }
    crate::xdg::app_data_dir()
        .join("run")
        .join(SOCKET_FILE)
}

pub fn ensure_parent(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path has no parent directory",
        ));
    };
    fs::create_dir_all(parent)
}

pub fn prepare_listener_socket(path: &Path) -> io::Result<()> {
    ensure_parent(path)?;
    if !path.exists() {
        return Ok(());
    }

    match UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("socket is already in use: {}", path.display()),
        )),
        Err(_) => fs::remove_file(path),
    }
}
