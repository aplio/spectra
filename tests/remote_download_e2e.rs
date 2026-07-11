#![cfg(unix)]

//! `--remote` cross-platform e2e without real ssh or network: the
//! `SPECTRA_REMOTE_SSH_CMD` seam swaps `ssh -T -- <host>` for a local
//! `env ... sh -c` wrapper whose PATH is prepended with stub `uname` and
//! `curl` binaries. The stub `uname` reports a platform that differs from
//! the local build, so the bridge takes the GitHub-download path; the stub
//! `curl` serves a prepared tarball of the real spectra binary instead of
//! hitting the network.

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
        "could not locate spectra binary for remote download e2e test",
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

fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).expect("write stub script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub script");
}

/// A `uname -s`/`-m` pair that differs from the local build but still maps
/// to a published release target (same OS, flipped architecture).
fn mismatched_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "Darwin"
    } else {
        "Linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "x86_64"
    } else {
        "aarch64"
    };
    (os, arch)
}

#[test]
fn remote_attach_downloads_release_binary_on_platform_mismatch() {
    let bin = resolve_spectra_binary().expect("resolve spectra binary");

    // "Remote host" environment in tempdirs. No spectra is installed there:
    // the bridge must download the release tarball via the stub curl.
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_runtime = remote.path().join("runtime");
    let remote_data = remote.path().join("data");
    let remote_config = remote.path().join("config");
    let fake_home = remote.path().join("home");
    let stub_bin = remote.path().join("stubbin");
    for dir in [
        &remote_runtime,
        &remote_data,
        &remote_config,
        &fake_home,
        &stub_bin,
    ] {
        std::fs::create_dir_all(dir).expect("create remote env dir");
        assert_no_whitespace(dir);
    }

    // Release tarball: the real binary packaged like the release workflow
    // does (`tar -czf ... -C target/release spectra`).
    let staging = remote.path().join("staging");
    std::fs::create_dir_all(&staging).expect("create staging dir");
    std::fs::copy(&bin, staging.join("spectra")).expect("stage binary");
    let tarball = remote.path().join("release.tar.gz");
    let tar_status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg("spectra")
        .status()
        .expect("run tar");
    assert!(tar_status.success(), "packaging the release tarball failed");

    // Stub uname reports a platform that differs from the local build so the
    // bridge cannot seed the local binary and must download instead.
    let (remote_os, remote_arch) = mismatched_platform();
    write_executable(
        &stub_bin.join("uname"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n-s) echo {remote_os};;\n-m) echo {remote_arch};;\nesac\n"
        ),
    );

    // Stub curl copies the prepared tarball to -o and logs the requested URL.
    let curl_log = remote.path().join("curl.log");
    write_executable(
        &stub_bin.join("curl"),
        concat!(
            "#!/bin/sh\n",
            "out=\n",
            "url=\n",
            "while [ $# -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    -o) out=$2; shift 2;;\n",
            "    -*) shift;;\n",
            "    *) url=$1; shift;;\n",
            "  esac\n",
            "done\n",
            "echo \"$url\" >>\"$SPECTRA_TEST_CURL_LOG\"\n",
            "cp \"$SPECTRA_TEST_TARBALL\" \"$out\"\n",
        ),
    );

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

    // Fake ssh: local `env ... sh -c` wrapper with the stub dir first on
    // PATH so the probe sees the stub uname and the download uses stub curl.
    let fake_ssh = format!(
        "env HOME={} XDG_RUNTIME_DIR={} XDG_DATA_HOME={} XDG_CONFIG_HOME={} PATH={}:/usr/bin:/bin SPECTRA_TEST_TARBALL={} SPECTRA_TEST_CURL_LOG={} sh -c",
        fake_home.display(),
        remote_runtime.display(),
        remote_data.display(),
        remote_config.display(),
        stub_bin.display(),
        tarball.display(),
        curl_log.display(),
    );
    // SAFETY: This is the only test in this binary, and the variable is set
    // before any bridge threads are spawned.
    unsafe { std::env::set_var("SPECTRA_REMOTE_SSH_CMD", &fake_ssh) };

    let bridge = remote::start_bridge("dummyhost").expect("start bridge listener");

    // start_bridge must have downloaded the tarball into the seeded path.
    let seeded = fake_home.join(remote::REMOTE_SEEDED_BINARY_SUFFIX);
    assert!(
        seeded.exists(),
        "downloaded binary should exist at {}",
        seeded.display()
    );
    let seeded_meta = std::fs::metadata(&seeded).expect("downloaded binary metadata");
    assert_eq!(
        seeded_meta.len(),
        std::fs::metadata(&bin)
            .expect("local binary metadata")
            .len(),
        "downloaded binary should match the tarball contents"
    );
    assert_eq!(
        seeded_meta.permissions().mode() & 0o777,
        0o755,
        "downloaded binary must be executable"
    );
    let seeded_mtime = seeded_meta.modified().expect("downloaded binary mtime");

    // The stub curl was asked for the versioned release asset for the remote
    // platform, not the local one.
    let version = env!("CARGO_PKG_VERSION");
    let requested = std::fs::read_to_string(&curl_log).expect("read curl log");
    let target = match (remote_os, remote_arch) {
        ("Linux", "x86_64") => "linux-x86_64",
        ("Linux", "aarch64") => "linux-aarch64",
        ("Darwin", "aarch64") => "macos-arm64",
        ("Darwin", "x86_64") => "macos-x86_64",
        other => panic!("unexpected mismatched platform: {other:?}"),
    };
    assert_eq!(
        requested.trim(),
        format!(
            "https://github.com/aplio/spectra/releases/download/v{version}/spectra-v{version}-{target}.tar.gz"
        )
    );

    // Drive the bridge socket like the local client would: the downloaded
    // binary must actually run the remote bridge.
    let mut stream = UnixStream::connect(bridge.socket_path()).expect("connect to bridge socket");
    stream.set_nonblocking(true).expect("set nonblocking");
    let hello = encode_message(&ClientMessage::Hello {
        cols: 80,
        rows: 24,
        attach_target: None,
        client_identity: None,
        protocol_version: Some(PROTOCOL_VERSION),
        host_colors: None,
    })
    .expect("encode hello");
    stream.write_all(&hello).expect("send hello");

    wait_for_render(&mut stream, REMOTE_WAIT_TIMEOUT)
        .expect("render should arrive through fake ssh transport");
    drop(stream);
    drop(bridge);

    // A fresh bridge start sees a matching version and skips re-downloading.
    drop(remote::start_bridge("dummyhost").expect("restart bridge listener"));
    assert_eq!(
        std::fs::read_to_string(&curl_log)
            .expect("read curl log after reuse")
            .lines()
            .count(),
        1,
        "matching version must not be re-downloaded"
    );
    assert_eq!(
        std::fs::metadata(&seeded)
            .expect("downloaded binary metadata after reuse")
            .modified()
            .expect("downloaded binary mtime after reuse"),
        seeded_mtime,
        "matching version must not be re-installed"
    );
}
