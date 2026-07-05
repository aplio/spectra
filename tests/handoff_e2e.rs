//! End-to-end tests for the live server handoff (`spectra server-handoff`):
//! a new server process takes over the running server's PTYs via SCM_RIGHTS
//! without killing the pane processes.

#![cfg(unix)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(6);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(20);

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Kills the handed-off (successor) server, which is not our direct child.
struct PidKillGuard {
    pid: u32,
}

impl Drop for PidKillGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status();
    }
}

struct TestEnv {
    _dir: tempfile::TempDir,
    runtime_dir: PathBuf,
    data_home: PathBuf,
    config_home: PathBuf,
}

fn test_env() -> TestEnv {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    let config_home = dir.path().join("config-home");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    std::fs::create_dir_all(&config_home).expect("create config dir");
    TestEnv {
        _dir: dir,
        runtime_dir,
        data_home,
        config_home,
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
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for handoff e2e test",
    ))
}

fn spectra_command(env: &TestEnv) -> io::Result<Command> {
    let bin = resolve_spectra_binary()?;
    let mut command = Command::new(bin);
    command
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .env("XDG_DATA_HOME", &env.data_home)
        .env("XDG_CONFIG_HOME", &env.config_home);
    Ok(command)
}

fn spawn_server(env: &TestEnv) -> io::Result<ServerProcess> {
    let child = spectra_command(env)?
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("cat")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(ServerProcess { child })
}

fn client_socket_path(env: &TestEnv) -> PathBuf {
    env.runtime_dir.join("spectra").join("spectra.sock")
}

fn api_socket_path(env: &TestEnv) -> PathBuf {
    env.runtime_dir.join("spectra").join("spectra-api.sock")
}

fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for socket file: {}",
            socket.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

struct ApiClient {
    reader: BufReader<UnixStream>,
}

impl ApiClient {
    fn connect(socket: &Path) -> io::Result<Self> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            err.kind(),
                            format!("timed out connecting to api socket: {}", socket.display()),
                        ));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        };
        stream.set_read_timeout(Some(WAIT_TIMEOUT))?;
        stream.set_write_timeout(Some(WAIT_TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    fn request(&mut self, request: &str) -> io::Result<Value> {
        let stream = self.reader.get_mut();
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        serde_json::from_str(line.trim_end()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid api response line {line:?}: {err}"),
            )
        })
    }
}

fn wait_for_pane_text(env: &TestEnv, needle: &str) -> String {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        // Reconnect each attempt so this also works across the handoff gap.
        if let Ok(mut client) = ApiClient::connect(&api_socket_path(env))
            && let Ok(read) =
                client.request(r#"{"id":9,"method":"pane.read","params":{"pane_id":1}}"#)
            && let Some(text) = read["result"]["text"].as_str()
            && text.contains(needle)
        {
            return text.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "pane text never contained {needle:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn send_keys(env: &TestEnv, text: &str) {
    let mut client = ApiClient::connect(&api_socket_path(env)).expect("connect api client");
    let request = serde_json::json!({
        "id": 1,
        "method": "pane.send_keys",
        "params": {"pane_id": 1, "text": text},
    });
    let response = client.request(&request.to_string()).expect("send_keys");
    assert_eq!(
        response["result"]["ok"], true,
        "send_keys failed: {response}"
    );
}

/// First child pid of `pid` via /proc (Linux). The test server hosts one
/// pane running `cat`, so its only child is the pane process.
#[cfg(target_os = "linux")]
fn first_child_pid(pid: u32) -> u32 {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Some(first) = content.split_whitespace().next()
            && let Ok(child) = first.parse::<u32>()
        {
            return child;
        }
        assert!(
            Instant::now() < deadline,
            "server {pid} never reported a pane child in {path}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "linux")]
fn process_cmdline(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| raw.replace('\0', " ").trim().to_string())
}

// Asserts pane processes survive a handoff by inspecting the server's child
// process via /proc, so it only runs on Linux (macOS has no /proc). The other
// handoff e2e tests below are platform-agnostic and still run everywhere.
#[cfg(target_os = "linux")]
#[test]
fn handoff_moves_running_panes_to_a_new_server_without_killing_them() {
    let env = test_env();
    let mut server_a = spawn_server(&env).expect("spawn server A");
    wait_for_socket(&api_socket_path(&env));

    // Put recognizable content on the pane's screen (cat echoes its input).
    send_keys(&env, "pre-handoff-marker\n");
    wait_for_pane_text(&env, "pre-handoff-marker");

    let pane_pid = first_child_pid(server_a.child.id());
    let pane_cmdline = process_cmdline(pane_pid).expect("pane child cmdline");
    assert!(
        pane_cmdline.contains("cat"),
        "expected the pane child to run cat, got {pane_cmdline:?}"
    );

    // Run the handoff with the same environment; the coordinator reports
    // success only after the successor server is listening.
    let output = spectra_command(&env)
        .expect("build spectra command")
        .arg("server-handoff")
        .stdin(Stdio::null())
        .output()
        .expect("run server-handoff");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "server-handoff failed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("server handoff complete"),
        "unexpected coordinator output: {stdout:?}"
    );
    let successor_pid: u32 = stdout
        .split("(pid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|pid| pid.trim().parse().ok())
        .expect("successor pid in coordinator output");
    let _kill_successor = PidKillGuard { pid: successor_pid };

    // The old server must exit on its own after the handoff.
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    loop {
        if server_a
            .child
            .try_wait()
            .expect("try_wait server A")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "old server did not exit after the handoff"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // The successor serves the sockets and the replayed screen content.
    wait_for_socket(&api_socket_path(&env));
    let text = wait_for_pane_text(&env, "pre-handoff-marker");
    assert!(
        text.contains("pre-handoff-marker"),
        "replayed screen missing marker: {text:?}"
    );

    // The pane child survived the handoff: same pid, still running cat.
    let cmdline_after = process_cmdline(pane_pid)
        .unwrap_or_else(|| panic!("pane child {pane_pid} died across the handoff"));
    assert!(
        cmdline_after.contains("cat"),
        "pane pid {pane_pid} was recycled: {cmdline_after:?}"
    );

    // A write→read round-trip through the successor reaches the SAME cat
    // process: input sent via the new server comes back as echoed output.
    send_keys(&env, "post-handoff-roundtrip\n");
    wait_for_pane_text(&env, "post-handoff-roundtrip");
}

#[test]
fn handoff_is_refused_while_a_client_is_attached_and_server_keeps_serving() {
    let env = test_env();
    let _server = spawn_server(&env).expect("spawn server");
    wait_for_socket(&client_socket_path(&env));
    wait_for_socket(&api_socket_path(&env));

    // Attach a raw client with a real Hello handshake and wait for the
    // first render so the attachment is fully registered.
    let mut client = UnixStream::connect(client_socket_path(&env)).expect("connect client");
    client
        .set_read_timeout(Some(WAIT_TIMEOUT))
        .expect("client read timeout");
    let hello =
        spectra::ipc::codec::encode_message(&spectra::ipc::protocol::ClientMessage::Hello {
            cols: 80,
            rows: 24,
            attach_target: None,
            client_identity: Some("handoff-e2e".to_string()),
            protocol_version: Some(spectra::ipc::protocol::PROTOCOL_VERSION),
            host_colors: None,
        })
        .expect("encode hello");
    client.write_all(&hello).expect("send hello");
    let mut first_bytes = [0u8; 1];
    client
        .read_exact(&mut first_bytes)
        .expect("server response to hello");

    // server.handoff must be refused with a clear client count.
    let mut api = ApiClient::connect(&api_socket_path(&env)).expect("connect api");
    let response = api
        .request(r#"{"id":1,"method":"server.handoff"}"#)
        .expect("handoff response");
    let message = response["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected refusal error, got {response}"));
    assert!(
        message.contains("client(s) attached"),
        "unexpected refusal message: {message:?}"
    );
    assert_eq!(response["error"]["code"], -32001, "got: {response}");

    // Absolute rule: after a refused handoff the server keeps serving.
    send_keys(&env, "still-serving\n");
    wait_for_pane_text(&env, "still-serving");
    assert!(
        client_socket_path(&env).exists(),
        "client socket vanished after refused handoff"
    );
}

#[test]
fn handoff_without_a_running_server_reports_a_clear_error() {
    let env = test_env();
    let output = spectra_command(&env)
        .expect("build spectra command")
        .arg("server-handoff")
        .stdin(Stdio::null())
        .output()
        .expect("run server-handoff");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no spectra server is running"),
        "unexpected stderr: {stderr:?}"
    );
}
