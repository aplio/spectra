#![cfg(unix)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(6);

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

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for remote bridge e2e test",
    ))
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

fn setup_server_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");
    (dir, runtime_dir, data_home)
}

struct RawClient {
    stream: UnixStream,
    read_buffer: Vec<u8>,
}

impl RawClient {
    fn connect(socket: &Path) -> io::Result<Self> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(err);
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        };
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            read_buffer: Vec::new(),
        })
    }

    fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
        let encoded = encode_message(message)?;
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut offset = 0usize;
        while offset < encoded.len() {
            match self.stream.write(&encoded[offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero"));
                }
                Ok(n) => offset += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "write timeout"));
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn wait_for_message<F>(
        &mut self,
        timeout: Duration,
        mut predicate: F,
    ) -> io::Result<ServerMessage>
    where
        F: FnMut(&ServerMessage) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            for message in self.read_messages()? {
                if predicate(&message) {
                    return Ok(message);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for server message",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_messages(&mut self) -> io::Result<Vec<ServerMessage>> {
        let mut chunk = [0u8; 8192];
        let mut closed = false;
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => self.read_buffer.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }

        let decoded = decode_messages::<ServerMessage>(&mut self.read_buffer);
        if let Some(error) = decoded.errors.first() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid server frame: {error}"),
            ));
        }
        if closed && decoded.messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server closed socket",
            ));
        }
        Ok(decoded.messages)
    }
}

fn hello(protocol_version: Option<u32>) -> ClientMessage {
    ClientMessage::Hello {
        cols: 80,
        rows: 24,
        attach_target: None,
        client_identity: None,
        protocol_version,
    }
}

#[test]
fn hello_with_mismatched_protocol_version_is_rejected_and_disconnected() {
    let (_dir, runtime_dir, data_home) = setup_server_env();
    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = RawClient::connect(&socket).expect("connect client");
    client
        .send(&hello(Some(PROTOCOL_VERSION + 1)))
        .expect("send mismatched hello");

    let message = client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Error { .. })
        })
        .expect("protocol mismatch error");
    match message {
        ServerMessage::Error { message } => {
            assert!(
                message.contains("protocol version mismatch"),
                "unexpected error message: {message}"
            );
            assert!(
                message.contains(&format!("client {}", PROTOCOL_VERSION + 1)),
                "error should name the client version: {message}"
            );
            assert!(
                message.contains(&format!("server {PROTOCOL_VERSION}")),
                "error should name the server version: {message}"
            );
        }
        other => panic!("expected error message, got {other:?}"),
    }

    let disconnect = client
        .wait_for_message(WAIT_TIMEOUT, |_| false)
        .expect_err("client should be disconnected after mismatch");
    assert_eq!(
        disconnect.kind(),
        io::ErrorKind::BrokenPipe,
        "unexpected post-error state: {disconnect}"
    );
}

#[test]
fn legacy_hello_without_protocol_version_still_attaches() {
    let (_dir, runtime_dir, data_home) = setup_server_env();
    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = RawClient::connect(&socket).expect("connect client");
    client.send(&hello(None)).expect("send legacy hello");

    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("legacy client should still receive a render");
}

#[test]
fn bridge_subcommand_relays_ndjson_between_stdio_and_server() {
    let (_dir, runtime_dir, data_home) = setup_server_env();
    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let bin = resolve_spectra_binary().expect("resolve binary");
    let config_home = data_home.join("config-home");
    let mut bridge = Command::new(bin)
        .arg("remote-client-bridge")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge subcommand");

    let mut bridge_stdin = bridge.stdin.take().expect("bridge stdin");
    let bridge_stdout = bridge.stdout.take().expect("bridge stdout");

    // Decode NDJSON lines from the bridge stdout on a helper thread so the
    // test itself never blocks on a pipe read.
    let (sender, receiver) = mpsc::channel::<ServerMessage>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(bridge_stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<ServerMessage>(&line) else {
                break;
            };
            if sender.send(message).is_err() {
                break;
            }
        }
    });

    let encoded = encode_message(&hello(Some(PROTOCOL_VERSION))).expect("encode hello");
    bridge_stdin.write_all(&encoded).expect("write hello line");
    bridge_stdin.flush().expect("flush hello line");

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for a render through the bridge");
        }
        match receiver.recv_timeout(remaining) {
            Ok(ServerMessage::Render { .. }) => break,
            Ok(_other) => continue,
            Err(err) => panic!("bridge stdout closed before a render arrived: {err}"),
        }
    }

    // Closing the bridge stdin must propagate EOF and terminate the bridge.
    drop(bridge_stdin);
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let status = loop {
        if let Some(status) = bridge.try_wait().expect("poll bridge exit") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = bridge.kill();
            panic!("bridge did not exit after stdin closed");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "bridge exited unsuccessfully: {status}");
    reader_thread.join().expect("join reader thread");
}
