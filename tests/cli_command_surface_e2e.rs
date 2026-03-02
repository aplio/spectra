#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);

struct CmdOutput {
    stdout: String,
    stderr: String,
    success: bool,
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
        "could not locate spectra binary for cli e2e test",
    ))
}

fn run_spectra(
    bin: &Path,
    runtime_dir: &Path,
    data_home: &Path,
    args: &[&str],
) -> io::Result<CmdOutput> {
    let output = Command::new(bin)
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_DATA_HOME", data_home)
        .output()?;

    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
    })
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

fn spawn_server(bin: &Path, runtime_dir: &Path, data_home: &Path) -> io::Result<Child> {
    Command::new(bin)
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
        .spawn()
}

#[test]
fn command_surface_bootstrap_new_session_and_ls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let created =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-session"]).expect("run new-session");
    assert!(created.success, "stderr: {}", created.stderr);
    assert!(created.stdout.contains("session created"));

    let listed = run_spectra(&bin, &runtime_dir, &data_home, &["ls"]).expect("run ls");
    assert!(listed.success, "stderr: {}", listed.stderr);

    let lines = listed
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "unexpected ls output: {}", listed.stdout);
    assert!(lines[0].contains("s1"), "line: {}", lines[0]);
    assert!(lines[0].contains("windows=1"), "line: {}", lines[0]);

    let killed = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s1"],
    )
    .expect("kill last session");
    assert!(killed.success, "stderr: {}", killed.stderr);
}

#[test]
fn command_surface_mutations_and_selectors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let bootstrap =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-session"]).expect("bootstrap session");
    assert!(bootstrap.success, "stderr: {}", bootstrap.stderr);

    let new_window =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-window"]).expect("new-window");
    assert!(new_window.success, "stderr: {}", new_window.stderr);
    assert!(new_window.stdout.contains("window created"));

    let split = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["split-window", "--horizontal"],
    )
    .expect("split-window");
    assert!(split.success, "stderr: {}", split.stderr);
    assert!(split.stdout.contains("window split"));

    let select_window = run_spectra(&bin, &runtime_dir, &data_home, &["select-window", "1"])
        .expect("select-window");
    assert!(select_window.success, "stderr: {}", select_window.stderr);
    assert!(select_window.stdout.contains("window selected"));

    let select_pane =
        run_spectra(&bin, &runtime_dir, &data_home, &["select-pane", "1"]).expect("select-pane");
    assert!(select_pane.success, "stderr: {}", select_pane.stderr);
    assert!(select_pane.stdout.contains("pane selected"));

    let new_session = run_spectra(&bin, &runtime_dir, &data_home, &["new-session"])
        .expect("create second session");
    assert!(new_session.success, "stderr: {}", new_session.stderr);
    assert!(new_session.stdout.contains("session created"));

    let listed = run_spectra(&bin, &runtime_dir, &data_home, &["ls"]).expect("ls");
    assert!(listed.success, "stderr: {}", listed.stderr);
    let lines = listed
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "unexpected ls output: {}", listed.stdout);
    assert!(lines.iter().any(|line| line.contains("s1")));
    assert!(lines.iter().any(|line| line.contains("s2")));

    let kill_second = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s2"],
    )
    .expect("kill s2");
    assert!(kill_second.success, "stderr: {}", kill_second.stderr);
    assert!(kill_second.stdout.contains("session killed"));

    let kill_last = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s1"],
    )
    .expect("kill s1");
    assert!(kill_last.success, "stderr: {}", kill_last.stderr);
    assert!(kill_last.stdout.contains("server shutting down"));
}

#[test]
fn command_surface_send_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let bootstrap =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-session"]).expect("bootstrap session");
    assert!(bootstrap.success, "stderr: {}", bootstrap.stderr);

    let sent = run_spectra(&bin, &runtime_dir, &data_home, &["send-keys", "echo", "ok"])
        .expect("send-keys");
    assert!(sent.success, "stderr: {}", sent.stderr);
    assert!(sent.stdout.contains("keys sent to 1 pane(s)"));

    let invalid_target = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["send-keys", "--target", "missing", "echo", "nope"],
    )
    .expect("invalid target");
    assert!(!invalid_target.success, "stdout: {}", invalid_target.stdout);
    assert!(
        invalid_target
            .stderr
            .contains("session `missing` not found")
    );

    let empty =
        run_spectra(&bin, &runtime_dir, &data_home, &["send-keys", ""]).expect("send empty text");
    assert!(!empty.success, "stdout: {}", empty.stdout);
    assert!(empty.stderr.contains("send-keys text cannot be empty"));

    let conflict = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["send-keys", "--target", "s1", "--all", "echo", "hi"],
    )
    .expect("send-keys selector conflict");
    assert!(!conflict.success, "stdout: {}", conflict.stdout);
    assert!(conflict.stderr.contains("cannot be used with"));

    let kill_last = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s1"],
    )
    .expect("kill s1");
    assert!(kill_last.success, "stderr: {}", kill_last.stderr);
}

#[test]
fn command_surface_source_file_reload_reports_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let bootstrap =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-session"]).expect("bootstrap session");
    assert!(bootstrap.success, "stderr: {}", bootstrap.stderr);

    let config_path = dir.path().join("reload.toml");
    std::fs::write(
        &config_path,
        r#"
prefix = "C-a"

[shell]
suppress_prompt_eol_marker = false
"#,
    )
    .expect("write config");

    let reloaded = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &[
            "source-file",
            config_path.to_str().expect("config path utf-8"),
        ],
    )
    .expect("run source-file");
    assert!(reloaded.success, "stderr: {}", reloaded.stderr);
    assert!(reloaded.stdout.contains("config reloaded"));

    let broken_path = dir.path().join("broken.toml");
    std::fs::write(&broken_path, "prefix = [").expect("write invalid config");
    let failed = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &[
            "source-file",
            broken_path.to_str().expect("broken path utf-8"),
        ],
    )
    .expect("run source-file invalid");
    assert!(!failed.success, "stdout: {}", failed.stdout);
    assert!(failed.stderr.contains("source-file failed"));

    let kill_last = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s1"],
    )
    .expect("kill s1");
    assert!(kill_last.success, "stderr: {}", kill_last.stderr);
}

#[test]
fn command_surface_restores_runtime_state_after_server_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    let bin = resolve_spectra_binary().expect("resolve binary");

    let mut server = spawn_server(&bin, &runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for server socket");

    let new_window =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-window"]).expect("create second window");
    assert!(new_window.success, "stderr: {}", new_window.stderr);

    let split = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["split-window", "--horizontal"],
    )
    .expect("split second window");
    assert!(split.success, "stderr: {}", split.stderr);

    let new_session =
        run_spectra(&bin, &runtime_dir, &data_home, &["new-session"]).expect("new session");
    assert!(new_session.success, "stderr: {}", new_session.stderr);

    let before = run_spectra(&bin, &runtime_dir, &data_home, &["ls"]).expect("ls before restart");
    assert!(before.success, "stderr: {}", before.stderr);
    let before_lines = before
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        before_lines.len(),
        2,
        "unexpected ls output: {}",
        before.stdout
    );

    server.kill().expect("kill running server");
    let _ = server.wait();

    let after = run_spectra(&bin, &runtime_dir, &data_home, &["ls"]).expect("ls after restart");
    assert!(after.success, "stderr: {}", after.stderr);
    let after_lines = after
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        after_lines.len(),
        2,
        "unexpected ls output: {}",
        after.stdout
    );
    assert!(
        after_lines.iter().any(|line| line.contains("s1")
            && line.contains("windows=2")
            && line.contains("panes=3")
            && line.contains("focus=w2.p3")),
        "missing restored s1 layout in output: {}",
        after.stdout
    );
    assert!(
        after_lines.iter().any(|line| line.starts_with("* s2")
            && line.contains("windows=1")
            && line.contains("panes=1")
            && line.contains("focus=w1.p1")),
        "missing restored active s2 in output: {}",
        after.stdout
    );

    let kill_s2 = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s2"],
    )
    .expect("kill s2");
    assert!(kill_s2.success, "stderr: {}", kill_s2.stderr);

    let kill_s1 = run_spectra(
        &bin,
        &runtime_dir,
        &data_home,
        &["kill-session", "--target", "s1"],
    )
    .expect("kill s1");
    assert!(kill_s1.success, "stderr: {}", kill_s1.stderr);
}
