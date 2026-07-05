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
