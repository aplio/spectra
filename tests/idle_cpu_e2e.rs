//! Regression gate for the readiness-based server loop: an idle server must
//! not burn CPU. The old 1ms busy-poll loop consumed ~150ms CPU per 10s of
//! idle (release build); the polling-based loop consumes ~3ms. The bound
//! here is deliberately lenient (debug builds, CI jitter) while still
//! catching any return to busy-polling by an order of magnitude.

#![cfg(target_os = "linux")]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
/// How long the server is left completely idle while CPU time is sampled.
const IDLE_WINDOW: Duration = Duration::from_secs(5);
/// Maximum on-CPU time the server may accumulate over [`IDLE_WINDOW`].
/// The busy-poll loop burned well over 100ms in this window even in
/// release builds; the readiness-based loop stays in single-digit ms.
const MAX_IDLE_CPU: Duration = Duration::from_millis(50);

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_server(runtime_dir: &Path, data_home: &Path) -> io::Result<ServerProcess> {
    let bin = resolve_spectra_binary()?;
    let config_home = data_home.join("config-home");
    std::fs::create_dir_all(&config_home)?;

    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("cat")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(ServerProcess { child })
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

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for idle cpu e2e test",
    ))
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

/// On-CPU time of `pid` from `/proc/<pid>/schedstat` (first field,
/// nanoseconds). `None` when scheduler stats are unavailable on this kernel.
fn on_cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/schedstat")).ok()?;
    let nanos: u64 = stat.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

#[test]
fn idle_server_burns_almost_no_cpu() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let runtime_dir = temp.path().join("runtime");
    let data_home = temp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data home");

    let server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let pid = server.child.id();
    wait_for_socket(&runtime_dir.join("spectra").join("spectra.sock")).expect("server socket");

    // Let startup work (shell spawn, initial render, plugin scan) finish
    // before sampling, so only steady-state idle cost is measured.
    thread::sleep(Duration::from_secs(2));

    let Some(before) = on_cpu_time(pid) else {
        eprintln!("skipping: /proc/<pid>/schedstat unavailable on this kernel");
        return;
    };
    thread::sleep(IDLE_WINDOW);
    let after = on_cpu_time(pid).expect("schedstat readable for running server");

    let burned = after.saturating_sub(before);
    assert!(
        burned <= MAX_IDLE_CPU,
        "idle server burned {burned:?} CPU over {IDLE_WINDOW:?} (limit {MAX_IDLE_CPU:?}); \
         the event loop is busy-polling again"
    );
}
