//! Full-stack coverage for the ghostty-style command-finish bell
//! (`[command_finish]`): a pane emitting OSC 133;C…D over a real server
//! must ring attached clients' host terminals with a BEL delivered as a
//! `ServerMessage::Passthrough` frame. The panes run `cat`, so pasting the
//! marks plays the role of a shell with OSC 133 integration — which is
//! also exactly what a remote shell over ssh looks like to spectra, since
//! escape sequences pass through the ssh byte stream unchanged.

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spectra::attach_target::AttachTarget;
use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{
    ClientMessage, CommandRequest, CommandSplitAxis, PROTOCOL_VERSION, ServerMessage,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(4);
const BELL: &str = "\x07";

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
    fn connect(socket: &Path, attach_target: Option<AttachTarget>) -> io::Result<Self> {
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
            cols: 80,
            rows: 24,
            attach_target,
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

    fn wait_for_bell(&mut self, timeout: Duration) -> io::Result<()> {
        self.wait_for_message(
            timeout,
            |message| matches!(message, ServerMessage::Passthrough { ansi } if ansi.contains(BELL)),
        )
        .map(|_| ())
    }

    /// Drain everything currently queued and assert no BEL passthrough is
    /// among it.
    fn assert_no_bell(&mut self) {
        for message in self.read_messages().expect("read pending messages") {
            if let ServerMessage::Passthrough { ansi } = &message {
                assert!(
                    !ansi.contains(BELL),
                    "unexpected bell passthrough: {ansi:?}"
                );
            }
        }
    }
}

/// Paste OSC 133 marks bracketing a "command" into the client's focused
/// pane. The panes run `cat`, so the pasted line is echoed back into the
/// pane's terminal state, where the marks are parsed — standing in for a
/// shell (local or at the far end of ssh) that emits OSC 133.
fn run_marked_command(client: &mut TestClient) {
    client
        .send(ClientMessage::Paste {
            text: "\x1b]133;C\x07command-output\r".to_string(),
        })
        .expect("paste 133;C");
    // A separate write so C and D usually arrive in distinct PTY reads and
    // the duration is measured across polls; a same-poll pair would still
    // ring (min_duration_ms = 0).
    thread::sleep(Duration::from_millis(50));
    client
        .send(ClientMessage::Paste {
            text: "\x1b]133;D;0\x07\r".to_string(),
        })
        .expect("paste 133;D");
}

fn spawn_server(dir: &Path, config: &str) -> io::Result<(ServerProcess, PathBuf)> {
    let runtime_dir = dir.join("runtime");
    let data_home = dir.join("data");
    let config_home = dir.join("config-home");
    std::fs::create_dir_all(&runtime_dir)?;
    std::fs::create_dir_all(&data_home)?;
    std::fs::create_dir_all(config_home.join("spectra"))?;
    std::fs::write(config_home.join("spectra").join("config.toml"), config)?;

    let bin = resolve_spectra_binary()?;
    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("cat")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let socket = runtime_dir.join("spectra").join("spectra.sock");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while !socket.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for socket file: {}", socket.display()),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
    Ok((ServerProcess { child }, socket))
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
    for candidate in [target_dir.join("spectra"), target_dir.join("spectra.exe")] {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for command-finish bell e2e test",
    ))
}

#[test]
fn command_finish_marks_ring_attached_clients_as_a_bell_passthrough() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, socket) = spawn_server(
        dir.path(),
        "[command_finish]\nnotify = \"always\"\nmin_duration_ms = 0\n",
    )
    .expect("spawn server");

    let mut client = TestClient::connect(&socket, None).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    run_marked_command(&mut client);
    client
        .wait_for_bell(WAIT_TIMEOUT)
        .expect("BEL passthrough should reach the attached client");
}

#[test]
fn unfocused_mode_rings_only_clients_viewing_another_pane() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, socket) = spawn_server(
        dir.path(),
        "[command_finish]\nnotify = \"unfocused\"\nmin_duration_ms = 0\n",
    )
    .expect("spawn server");

    // Split the first window so two clients can view different panes.
    let mut command_client = TestClient::connect(&socket, None).expect("connect command client");
    command_client
        .send(ClientMessage::Command {
            request: CommandRequest::SplitWindow {
                target: None,
                axis: CommandSplitAxis::Vertical,
            },
        })
        .expect("split window");
    command_client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::CommandResult { .. })
        })
        .expect("split result");
    drop(command_client);

    let watcher_target = AttachTarget::parse("s1:w1.p1").expect("parse watcher target");
    let runner_target = AttachTarget::parse("s1:w1.p2").expect("parse runner target");
    let mut watcher =
        TestClient::connect(&socket, Some(watcher_target)).expect("connect watcher client");
    let mut runner =
        TestClient::connect(&socket, Some(runner_target)).expect("connect runner client");
    for client in [&mut watcher, &mut runner] {
        client
            .wait_for_message(WAIT_TIMEOUT, |message| {
                matches!(message, ServerMessage::Render { .. })
            })
            .expect("initial render");
    }

    // The command finishes in the runner's focused pane (p2): the watcher
    // (viewing p1) must be rung, the runner must not.
    run_marked_command(&mut runner);
    watcher
        .wait_for_bell(WAIT_TIMEOUT)
        .expect("the client viewing another pane should be rung");
    // The watcher's bell was queued in the same server pass as any
    // (wrong) runner bell would have been; a short grace period keeps
    // the check honest across the separate sockets.
    thread::sleep(Duration::from_millis(100));
    runner.assert_no_bell();
}
