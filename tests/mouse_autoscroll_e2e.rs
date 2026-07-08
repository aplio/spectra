//! Drag-selection edge auto-scroll through the real server loop: a drag
//! held on the pane's top row must keep scrolling into history without any
//! further mouse events (timer-driven via tick / next_deadline).

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{
    ClientMessage, NetMouseButton, NetMouseEvent, NetMouseEventKind, PROTOCOL_VERSION,
    ServerMessage,
};

// Only `ansi_bytes_to_rows` is used from the shared support module.
#[allow(dead_code)]
mod support;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(8);

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
    ansi: Vec<u8>,
}

impl TestClient {
    fn connect(socket: &Path) -> io::Result<Self> {
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
            ansi: Vec::new(),
        };
        client.send(ClientMessage::Hello {
            cols: COLS,
            rows: ROWS,
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

    fn send_mouse(&mut self, kind: NetMouseEventKind, column: u16, row: u16) -> io::Result<()> {
        self.send(ClientMessage::Mouse {
            mouse: NetMouseEvent {
                kind,
                column,
                row,
                modifiers: 0,
            },
        })
    }

    fn read_messages(&mut self) -> io::Result<Vec<ServerMessage>> {
        let mut chunk = [0u8; 8192];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
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
        Ok(decoded.messages)
    }

    fn screen_rows(&self) -> Vec<String> {
        support::render_snapshot::ansi_bytes_to_rows(&self.ansi, COLS as usize, ROWS as usize)
    }

    fn screen_contains(&self, needle: &str) -> bool {
        self.screen_rows().iter().any(|row| row.contains(needle))
    }

    fn wait_for_screen_containing(&mut self, needle: &str, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            for message in self.read_messages()? {
                if let ServerMessage::Render { ansi } = message {
                    self.ansi.extend_from_slice(ansi.as_bytes());
                }
            }
            if self.screen_contains(needle) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for screen to contain {needle:?}; screen:\n{}",
                        self.screen_rows().join("\n")
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn spawn_server(runtime_dir: &Path, data_home: &Path) -> io::Result<ServerProcess> {
    let bin = resolve_spectra_binary()?;
    let config_home = data_home.join("config-home");
    std::fs::create_dir_all(config_home.join("spectra"))?;
    // Drag selection is host-side mouse handling, which is off by default.
    std::fs::write(
        config_home.join("spectra").join("config.toml"),
        "[mouse]\nenabled = true\n",
    )?;

    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        // Trailing args are joined and run through the shell: 200 numbered
        // lines of scrollback, then stay alive on `cat`.
        .arg("seq -f line-%g 1 200; cat")
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
        "could not locate spectra binary for mouse autoscroll e2e test",
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

#[test]
fn drag_held_on_top_row_keeps_scrolling_into_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let _server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = runtime_dir.join("spectra").join("spectra.sock");
    wait_for_socket(&socket).expect("server socket");

    let mut client = TestClient::connect(&socket).expect("attach client");
    client
        .wait_for_screen_containing("line-200", WAIT_TIMEOUT)
        .expect("guest output reaches the live tail");
    assert!(
        !client.screen_contains("line-150"),
        "line-150 must start above the viewport for the scroll assertion to mean anything"
    );

    // Anchor mid-pane, then drag once onto the top row and hold: with no
    // further mouse events, the server's tick must keep scrolling the view
    // toward history and the deep line must scroll in.
    client
        .send_mouse(
            NetMouseEventKind::Down {
                button: NetMouseButton::Left,
            },
            40,
            10,
        )
        .expect("mouse down");
    client
        .send_mouse(
            NetMouseEventKind::Drag {
                button: NetMouseButton::Left,
            },
            40,
            0,
        )
        .expect("drag to top row");

    client
        .wait_for_screen_containing("line-150", WAIT_TIMEOUT)
        .expect("auto-scroll must reveal history while the drag rests on the top row");

    client
        .send_mouse(
            NetMouseEventKind::Up {
                button: NetMouseButton::Left,
            },
            40,
            0,
        )
        .expect("mouse up");
}
