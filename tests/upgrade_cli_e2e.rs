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
    let target_dir = deps_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "deps directory has no parent"))?;

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

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
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

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
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

    let output =
        run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "error").expect("run --update");

    assert!(
        !output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error:"), "unexpected stderr: {}", stderr);
    assert!(stderr.contains("mock"), "unexpected stderr: {}", stderr);
}

/// Pid printed by the handoff coordinator: "... new server (pid N); ...".
fn parse_successor_pid(stdout: &str) -> Option<u32> {
    let rest = stdout.split("(pid ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[test]
fn update_succeeds_while_server_is_active_and_hands_off_automatically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let _server = spawn_server(&bin, &runtime_dir, &data_home).expect("start server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("socket exists");
    // The handoff request goes through the API socket, which the server
    // binds after the client socket — wait for it too or a cold-cache start
    // races the auto-handoff into a connection-refused failure.
    let api_socket = runtime_dir.join("spectra").join("spectra-api.sock");
    wait_for_socket(&api_socket).expect("api socket exists");

    // Binary replacement is an inode swap, safe while the old server runs;
    // --update therefore succeeds and, with no clients attached, moves the
    // server onto the (mock-)installed binary via an automatic live handoff.
    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "has_update")
        .expect("run --update");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The handoff successor keeps running detached; kill it before asserting
    // so a failure cannot leak the process.
    if let Some(pid) = parse_successor_pid(&stdout) {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }

    assert!(
        output.status.success(),
        "expected --update to succeed while server is active, stdout: {} stderr: {}",
        stdout,
        format_output(&output.stderr)
    );
    assert!(
        stdout.contains("Upgraded spectra from"),
        "unexpected stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("Attempting live handoff"),
        "expected the auto-handoff attempt in stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("server handoff complete"),
        "expected the handoff to complete with no clients attached: {} stderr: {}",
        stdout,
        format_output(&output.stderr)
    );
}

#[test]
fn update_without_running_server_omits_handoff_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--update"], "has_update")
        .expect("run --update");

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("server-handoff"),
        "handoff hint must only appear with an active server: {}",
        stdout
    );
}

#[test]
fn check_reports_up_to_date_in_mock_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--check"], "up_to_date")
        .expect("run --check");

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Already up to date"),
        "unexpected stdout: {}",
        stdout
    );
}

#[test]
fn check_reports_available_update_without_installing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--check"], "has_update")
        .expect("run --check");

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Update available"),
        "unexpected stdout: {}",
        stdout
    );
    assert!(
        !stdout.contains("Upgraded"),
        "check must not install: {}",
        stdout
    );
}

#[test]
fn check_is_allowed_while_server_is_active() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let _server = spawn_server(&bin, &runtime_dir, &data_home).expect("start server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("socket exists");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--check"], "has_update")
        .expect("run --check");

    assert!(
        output.status.success(),
        "expected --check to succeed while server is active, stderr: {}",
        format_output(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Update available"),
        "unexpected stdout: {}",
        stdout
    );
}

#[test]
fn version_flag_prints_package_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let output = run_spectra(&bin, &runtime_dir, &data_home, &["--version"], "up_to_date")
        .expect("run --version");

    assert!(
        output.status.success(),
        "stderr: {}",
        format_output(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "unexpected stdout: {}",
        stdout
    );
}

fn format_output(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}
