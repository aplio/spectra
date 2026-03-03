#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use std::process::Output;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn resolve_spectra_binary() -> io::Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_spectra") {
        return Ok(PathBuf::from(path));
    }

    let current = std::env::current_exe()?;
    let deps_dir = current.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "test binary has no parent directory",
        )
    })?;
    let target_dir = deps_dir.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "deps directory has no parent")
    })?;

    let candidate = target_dir.join("spectra");
    if candidate.exists() {
        return Ok(candidate);
    }
    let candidate_exe = target_dir.join("spectra.exe");
    if candidate_exe.exists() {
        return Ok(candidate_exe);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for e2e test",
    ))
}

fn run_spectra(
    bin: &Path,
    runtime_dir: &Path,
    data_home: &Path,
    args: &[&str],
    state: &str,
) -> io::Result<Output> {
    Command::new(bin)
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_DATA_HOME", data_home)
        .env("SPECTRA_TEST_UPDATE_SOURCE", "mock")
        .env("SPECTRA_TEST_UPDATE_STATE", state)
        .output()
}

fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("spectra").join("spectra.sock")
}

fn wait_for_socket(socket: &Path) -> io::Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if socket.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for socket file: {}", socket.display()),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn spawn_server(bin: &Path, runtime_dir: &Path, data_home: &Path) -> io::Result<ServerProcess> {
    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("cat")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_DATA_HOME", data_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(ServerProcess { child })
}

#[test]
fn update_reports_up_to_date_in_mock_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "up_to_date")
        .expect("run --update");

    assert!(output.status.success(), "stderr: {}", format_output(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Already up to date"),
        "unexpected stdout: {}",
        stdout
    );
}

#[test]
fn update_reports_available_update_in_mock_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "has_update")
        .expect("run --update");

    assert!(output.status.success(), "stderr: {}", format_output(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Upgraded spectra from"),
        "unexpected stdout: {}",
        stdout
    );
}

#[test]
fn update_reports_failure_in_mock_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "error")
        .expect("run --update");

    assert!(!output.status.success(), "stderr: {}", format_output(&output.stderr));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error:"), "unexpected stderr: {}", stderr);
    assert!(stderr.contains("mock"), "unexpected stderr: {}", stderr);
}

#[test]
fn update_rejects_when_server_is_active() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let _server = spawn_server(&bin, &runtime_dir, &data_home).expect("start server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("socket exists");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "has_update")
        .expect("run --update");

    assert!(
        !output.status.success(),
        "expected --update to fail while server is active: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error:"), "unexpected stderr: {}", stderr);
    assert!(
        stderr.contains("active"),
        "unexpected stderr: {}",
        stderr
    );
}

fn format_output(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}
