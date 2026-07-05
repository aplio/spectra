#![cfg(unix)]

//! Full `--remote` bridge-listener e2e without real ssh: the
//! `SPECTRA_REMOTE_SSH_CMD` seam swaps the `ssh -T -- <host>` transport
//! prefix for a local `env ... sh -c` wrapper, so the composed
//! `sh -lc '<remote bridge command>'` runs on this machine against a
//! "remote" spectra server living in a tempdir environment.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};
use spectra::runtime::remote;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
// The fake ssh transport runs a login shell, so allow generous slack.
const REMOTE_WAIT_TIMEOUT: Duration = Duration::from_secs(12);

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
        "could not locate spectra binary for remote attach e2e test",
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

fn wait_for_render(stream: &mut UnixStream, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut read_buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "bridge closed the connection before a render arrived",
                ));
            }
            Ok(n) => read_buffer.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }

        let decoded = decode_messages::<ServerMessage>(&mut read_buffer);
        if let Some(error) = decoded.errors.first() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid server frame through fake ssh transport: {error}"),
            ));
        }
        for message in decoded.messages {
            match message {
                ServerMessage::Render { .. } => return Ok(()),
                ServerMessage::Error { message } => {
                    return Err(io::Error::other(format!("server error: {message}")));
                }
                ServerMessage::Shutdown { reason } => {
                    return Err(io::Error::other(format!("server shutdown: {reason}")));
                }
                _ => {}
            }
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for a render through the fake ssh transport",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_no_whitespace(path: &Path) {
    let display = path.display().to_string();
    assert!(
        !display.contains(char::is_whitespace),
        "test path must not contain whitespace (SPECTRA_REMOTE_SSH_CMD is whitespace-split): {display}"
    );
}

#[test]
fn remote_attach_bridges_hello_to_remote_server_through_fake_ssh() {
    let bin = resolve_spectra_binary().expect("resolve spectra binary");
    let bin_dir = bin.parent().expect("binary parent dir").to_path_buf();

    // "Remote host" environment in tempdirs.
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_runtime = remote.path().join("runtime");
    let remote_data = remote.path().join("data");
    let remote_config = remote.path().join("config");
    let fake_home = remote.path().join("home");
    let local_bin = fake_home.join(".local").join("bin");
    for dir in [&remote_runtime, &remote_data, &remote_config, &local_bin] {
        std::fs::create_dir_all(dir).expect("create remote env dir");
        assert_no_whitespace(dir);
    }

    // Install spectra in the fake home: `$HOME/.local/bin` fallback plus a
    // login-shell PATH entry so `command -v spectra` also resolves it.
    std::os::unix::fs::symlink(&bin, local_bin.join("spectra")).expect("symlink spectra");
    std::fs::write(
        fake_home.join(".profile"),
        format!("PATH=\"{}:$PATH\"\nexport PATH\n", bin_dir.display()),
    )
    .expect("write fake profile");

    // Pre-start the "remote" server with a deterministic shell.
    let server = Command::new(&bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("cat")
        .env("XDG_RUNTIME_DIR", &remote_runtime)
        .env("XDG_DATA_HOME", &remote_data)
        .env("XDG_CONFIG_HOME", &remote_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn remote server");
    let _server = ServerProcess { child: server };
    wait_for_socket(&remote_runtime.join("spectra").join("spectra.sock"))
        .expect("wait for remote server socket");

    // Fake ssh: local `env ... sh -c` wrapper instead of `ssh -T -- <host>`.
    let fake_ssh = format!(
        "env HOME={} XDG_RUNTIME_DIR={} XDG_DATA_HOME={} XDG_CONFIG_HOME={} sh -c",
        fake_home.display(),
        remote_runtime.display(),
        remote_data.display(),
        remote_config.display(),
    );
    // SAFETY: This is the only test in this binary, and the variable is set
    // before any bridge threads are spawned.
    unsafe { std::env::set_var("SPECTRA_REMOTE_SSH_CMD", &fake_ssh) };

    let bridge = remote::start_bridge("ssh://dummyhost").expect("start bridge listener");
    assert_eq!(
        bridge.host(),
        "dummyhost",
        "ssh:// scheme should be stripped"
    );
    let bridge_socket = bridge.socket_path().to_path_buf();
    let bridge_dir = bridge_socket
        .parent()
        .expect("bridge socket parent")
        .to_path_buf();
    let mode = std::fs::metadata(&bridge_dir)
        .expect("bridge dir metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "bridge dir must be private (0700)");

    // Drive the bridge socket like the local client would.
    let mut stream = UnixStream::connect(&bridge_socket).expect("connect to bridge socket");
    stream.set_nonblocking(true).expect("set nonblocking");
    let hello = encode_message(&ClientMessage::Hello {
        cols: 80,
        rows: 24,
        attach_target: None,
        client_identity: None,
        protocol_version: Some(PROTOCOL_VERSION),
    })
    .expect("encode hello");
    stream.write_all(&hello).expect("send hello");

    wait_for_render(&mut stream, REMOTE_WAIT_TIMEOUT)
        .expect("render should arrive through fake ssh transport");
    assert!(
        bridge.saw_remote_bytes(),
        "bridge should have observed remote bytes"
    );

    // Dropping the bridge stops the accept loop and removes the private dir.
    drop(stream);
    drop(bridge);
    assert!(
        !bridge_socket.exists(),
        "bridge socket should be removed on drop"
    );
    assert!(!bridge_dir.exists(), "bridge dir should be removed on drop");
}
