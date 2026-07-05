#![cfg(unix)]

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(4);

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
    let candidate_exe = target_dir.join("spectra.exe");
    if candidate_exe.exists() {
        return Ok(candidate_exe);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for api socket e2e test",
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

fn api_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("spectra").join("spectra-api.sock")
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

fn run_api_cli(
    runtime_dir: &Path,
    data_home: &Path,
    args: &[&str],
) -> io::Result<std::process::Output> {
    let bin = resolve_spectra_binary()?;
    let config_home = data_home.join("config-home");
    Command::new(bin)
        .arg("api")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .output()
}

#[test]
fn api_cli_round_trips_json_rpc_against_running_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let api_socket = api_socket_path(&runtime_dir);
    wait_for_socket(&api_socket).expect("wait for api socket");

    let output = run_api_cli(&runtime_dir, &data_home, &["session.list"]).expect("run spectra api");
    assert!(output.status.success(), "expected exit 0: {output:?}",);
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let result: Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    let sessions = result.as_array().expect("session.list result array");
    assert!(!sessions.is_empty(), "expected at least one session");
    assert!(sessions[0]["session_id"].is_string());

    // Unknown method: exit 1 with the server error message on stderr.
    let output =
        run_api_cli(&runtime_dir, &data_home, &["nosuch.method"]).expect("run spectra api");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("Error:") && stderr.contains("method not found"),
        "unexpected stderr: {stderr:?}"
    );

    // Invalid params JSON is a usage error (reported before connecting).
    let output = run_api_cli(&runtime_dir, &data_home, &["pane.read", "{not json"])
        .expect("run spectra api");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("invalid PARAMS_JSON"),
        "unexpected stderr: {stderr:?}"
    );
}

#[test]
fn api_cli_reports_missing_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let output = run_api_cli(&runtime_dir, &data_home, &["session.list"]).expect("run spectra api");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("no spectra server is running"),
        "unexpected stderr: {stderr:?}"
    );
}

#[test]
fn api_socket_serves_read_only_json_rpc_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let api_socket = api_socket_path(&runtime_dir);
    wait_for_socket(&api_socket).expect("wait for api socket");

    let mut client = ApiClient::connect(&api_socket).expect("connect api client");

    let sessions = client
        .request(r#"{"id":1,"method":"session.list"}"#)
        .expect("session.list response");
    assert_eq!(sessions["id"], 1);
    let session_entries = sessions["result"]
        .as_array()
        .expect("session.list result array");
    assert!(
        !session_entries.is_empty(),
        "expected at least one session: {sessions}"
    );
    assert!(session_entries[0]["session_id"].is_string());

    let panes = client
        .request(r#"{"id":2,"method":"pane.list"}"#)
        .expect("pane.list response");
    assert_eq!(panes["id"], 2);
    let pane_entries = panes["result"].as_array().expect("pane.list result array");
    assert!(
        !pane_entries.is_empty(),
        "expected at least one pane: {panes}"
    );
    let pane_id = pane_entries[0]["pane_id"]
        .as_u64()
        .expect("numeric pane id");

    let read = client
        .request(&format!(
            r#"{{"id":3,"method":"pane.read","params":{{"pane_id":{pane_id}}}}}"#
        ))
        .expect("pane.read response");
    assert_eq!(read["id"], 3);
    assert!(
        read["result"]["text"].is_string(),
        "expected result.text string: {read}"
    );

    // Garbage input gets a parse error but keeps the connection usable.
    let garbage = client
        .request("definitely not json")
        .expect("parse error response");
    assert!(garbage["id"].is_null());
    assert_eq!(garbage["error"]["code"], -32700);
    let after_garbage = client
        .request(r#"{"id":4,"method":"session.list"}"#)
        .expect("session.list after garbage");
    assert_eq!(after_garbage["id"], 4);
    assert!(after_garbage["result"].is_array());

    // A second concurrent connection is served independently.
    let mut second = ApiClient::connect(&api_socket).expect("connect second api client");
    let second_response = second
        .request(r#"{"id":"x","method":"session.list"}"#)
        .expect("second connection response");
    assert_eq!(second_response["id"], "x");
    assert!(second_response["result"].is_array());
}

impl ApiClient {
    /// Read one server-pushed line (event push after `events.subscribe`).
    fn read_line(&mut self) -> io::Result<Value> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        serde_json::from_str(line.trim_end()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid api event line {line:?}: {err}"),
            )
        })
    }
}

#[test]
fn api_socket_send_keys_effect_is_visible_via_pane_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let api_socket = api_socket_path(&runtime_dir);
    wait_for_socket(&api_socket).expect("wait for api socket");

    let mut client = ApiClient::connect(&api_socket).expect("connect api client");

    // The server pane runs `cat`, so sent text is echoed back into the pane.
    let sent = client
        .request(r#"{"id":1,"method":"pane.send_keys","params":{"pane_id":1,"text":"spectra-e2e-marker\n"}}"#)
        .expect("send_keys response");
    assert_eq!(sent["result"]["ok"], true, "unexpected response: {sent}");

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let read = client
            .request(r#"{"id":2,"method":"pane.read","params":{"pane_id":1}}"#)
            .expect("pane.read response");
        let text = read["result"]["text"].as_str().expect("pane text");
        if text.contains("spectra-e2e-marker") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("sent text never appeared in pane.read output: {text:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn api_socket_pushes_subscribed_events_to_other_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let api_socket = api_socket_path(&runtime_dir);
    wait_for_socket(&api_socket).expect("wait for api socket");

    let mut subscriber = ApiClient::connect(&api_socket).expect("connect subscriber");
    let subscribed = subscriber
        .request(r#"{"id":1,"method":"events.subscribe","params":{"events":["pane.split"]}}"#)
        .expect("subscribe response");
    assert_eq!(
        subscribed["result"]["subscribed"],
        serde_json::json!(["pane.split"]),
        "unexpected subscribe response: {subscribed}"
    );

    let mut actor = ApiClient::connect(&api_socket).expect("connect actor");
    let split = actor
        .request(r#"{"id":2,"method":"pane.split","params":{"axis":"vertical"}}"#)
        .expect("split response");
    let new_pane_id = split["result"]["pane_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("expected new pane id: {split}"));
    assert!(new_pane_id >= 2);

    let event = subscriber.read_line().expect("event line");
    assert_eq!(event["event"], "pane.split", "unexpected event: {event}");
    assert!(event["params"]["session_id"].is_string());
    assert!(event["params"]["pane_id"].is_u64());

    // The actor connection did not subscribe: a follow-up request still gets
    // its own response as the next line (no event interleaved before it).
    let panes = actor
        .request(r#"{"id":3,"method":"pane.list"}"#)
        .expect("pane.list response");
    assert_eq!(panes["id"], 3);
}

#[test]
fn api_cli_follow_prints_pushed_event_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let api_socket = api_socket_path(&runtime_dir);
    wait_for_socket(&api_socket).expect("wait for api socket");

    let bin = resolve_spectra_binary().expect("spectra binary");
    let config_home = data_home.join("config-home");
    let mut follower = Command::new(bin)
        .args(["api", "--follow", "events.subscribe"])
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn follower");

    // Stream the follower's stdout from a thread so we can wait with a
    // timeout for each line.
    let stdout = follower.stdout.take().expect("follower stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let first = rx.recv_timeout(WAIT_TIMEOUT).expect("subscription result");
    assert!(
        first.contains("subscribed"),
        "expected subscription result first, got {first:?}"
    );

    let mut actor = ApiClient::connect(&api_socket).expect("connect actor");
    actor
        .request(r#"{"id":1,"method":"pane.split","params":{"axis":"horizontal"}}"#)
        .expect("split response");

    let event_line = rx.recv_timeout(WAIT_TIMEOUT).expect("event line");
    let event: Value = serde_json::from_str(&event_line).expect("event line is JSON");
    assert_eq!(event["event"], "pane.split", "unexpected event: {event}");

    let _ = follower.kill();
    let _ = follower.wait();
    let _ = reader_thread.join();
}
