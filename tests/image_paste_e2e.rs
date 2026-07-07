#![cfg(unix)]

//! End-to-end test for clipboard image paste bridging: a paste-image action
//! makes the server ask the client for its clipboard image, the client
//! answers with the raw bytes, and the server stages them to a temp file
//! whose quoted path is pasted into the focused pane.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{
    ClientMessage, NetKeyEvent, PROTOCOL_VERSION, PasteImagePayload, ServerMessage,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(4);

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\ntiny-test-image-payload";

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestClient {
    stream: UnixStream,
    read_buffer: Vec<u8>,
}

impl TestClient {
    fn connect(socket: &Path, cols: u16, rows: u16) -> io::Result<Self> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            err.kind(),
                            format!("timed out connecting to socket: {}", socket.display()),
                        ));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        };
        stream.set_nonblocking(true)?;
        let mut client = Self {
            stream,
            read_buffer: Vec::new(),
        };
        client.send(ClientMessage::Hello {
            cols,
            rows,
            attach_target: None,
            client_identity: None,
            protocol_version: Some(PROTOCOL_VERSION),
            host_colors: None,
        })?;
        Ok(client)
    }

    fn send(&mut self, message: ClientMessage) -> io::Result<()> {
        let encoded = encode_message(&message)?;
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut offset = 0usize;
        while offset < encoded.len() {
            match self.stream.write(&encoded[offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "client socket write returned 0 bytes",
                    ));
                }
                Ok(n) => offset += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out writing client message",
                        ));
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn send_key(&mut self, key: KeyEvent) -> io::Result<()> {
        self.send(ClientMessage::Key {
            key: NetKeyEvent::from(key),
        })
    }

    fn send_prefixed_key(&mut self, ch: char) -> io::Result<()> {
        self.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
        self.send_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    fn send_quit(&mut self) -> io::Result<()> {
        self.send_prefixed_key('q')?;
        self.send_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
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

    fn wait_for_render_containing(&mut self, needle: &str, timeout: Duration) -> io::Result<()> {
        self.wait_for_message(timeout, |message| match message {
            ServerMessage::Render { ansi } => ansi.contains(needle),
            _ => false,
        })
        .map(|_| ())
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
                Ok(n) => {
                    self.read_buffer.extend_from_slice(&chunk[..n]);
                }
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
        "could not locate spectra binary for image paste e2e test",
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

fn shutdown(server: &mut ServerProcess, client: &mut TestClient) {
    // The default prefix is sticky and `prefix v` keeps it armed; clear it
    // so the quit chord below starts from a clean state.
    client
        .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("clear sticky prefix");
    client.send_quit().expect("send quit");
    let _ = client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Shutdown { .. })
        })
        .expect("shutdown event");

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(status) = server.child.try_wait().expect("check server exit") {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for server process to exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn paste_image_action_stages_image_and_pastes_its_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = runtime_dir.join("spectra").join("spectra.sock");
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = TestClient::connect(&socket, 100, 30).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    // `prefix v` runs the paste-image action; the server must round-trip a
    // clipboard read request to this client.
    client.send_prefixed_key('v').expect("send prefix v");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::PasteImageRequest)
        })
        .expect("paste image request");

    client
        .send(ClientMessage::PasteImage {
            image: Some(PasteImagePayload {
                format: "png".to_string(),
                data_base64: BASE64.encode(PNG_BYTES),
            }),
        })
        .expect("send paste image reply");

    // The pane runs `cat`, so the pasted quoted path is echoed back into
    // the pane and shows up in a render frame.
    client
        .wait_for_render_containing(".png", WAIT_TIMEOUT)
        .expect("staged image path visible in pane");

    // The staged file must exist under the runtime dir with the raw bytes.
    let staging_dir = runtime_dir.join("spectra").join("clipboard-images");
    let staged: Vec<PathBuf> = std::fs::read_dir(&staging_dir)
        .expect("staging dir exists")
        .map(|entry| entry.expect("staging dir entry").path())
        .collect();
    assert_eq!(staged.len(), 1, "exactly one staged image: {staged:?}");
    assert_eq!(
        staged[0].extension().and_then(|ext| ext.to_str()),
        Some("png")
    );
    assert_eq!(
        std::fs::read(&staged[0]).expect("read staged image"),
        PNG_BYTES
    );

    shutdown(&mut server, &mut client);
}

#[test]
fn paste_image_without_clipboard_image_reports_status_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = runtime_dir.join("spectra").join("spectra.sock");
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = TestClient::connect(&socket, 100, 30).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    client.send_prefixed_key('v').expect("send prefix v");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::PasteImageRequest)
        })
        .expect("paste image request");

    client
        .send(ClientMessage::PasteImage { image: None })
        .expect("send empty paste image reply");
    client
        .wait_for_render_containing("no image on clipboard", WAIT_TIMEOUT)
        .expect("status message for missing image");

    // Nothing may be staged or pasted into the pane.
    let staging_dir = runtime_dir.join("spectra").join("clipboard-images");
    assert!(
        !staging_dir.exists(),
        "no staging dir should be created for an empty reply"
    );

    shutdown(&mut server, &mut client);
}
