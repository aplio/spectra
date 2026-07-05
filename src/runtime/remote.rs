#![cfg(unix)]

//! Simplified remote attach over an ssh stdio bridge.
//!
//! Local side (`spectra --remote <host>`): a private per-invocation Unix
//! listener socket is created under the runtime dir, the normal interactive
//! client attaches to it, and every accepted connection is pumped through
//! `ssh -T` stdin/stdout to the remote host.
//!
//! Remote side (`spectra remote-client-bridge`, hidden subcommand): ensures a
//! server is running (same auto-spawn as a local attach), connects to the
//! local client socket, and relays raw protocol bytes between stdin/stdout
//! and the socket. stdout carries only protocol bytes; diagnostics go to
//! stderr.

use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::cli::Cli;
use crate::ipc::socket_path;
use crate::runtime::client;

/// Command executed on the remote host through `sh -lc` (login shell so PATH
/// customizations like `~/.local/bin` resolve). Falls back to the default
/// install location when `spectra` is not on PATH.
pub const REMOTE_BRIDGE_COMMAND: &str = "if command -v spectra >/dev/null 2>&1; then exec spectra remote-client-bridge; else exec \"$HOME/.local/bin/spectra\" remote-client-bridge; fi";

/// Test seam: when set, its whitespace-split value replaces the default
/// `ssh -T -- <host>` transport prefix. The composed `sh -lc '...'` remote
/// command is still appended as the final argument.
pub const REMOTE_SSH_CMD_ENV: &str = "SPECTRA_REMOTE_SSH_CMD";

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RELAY_CHUNK_BYTES: usize = 16 * 1024;

/// Entry point for `spectra --remote <host>` on the local machine.
pub fn run(cli: Cli) -> io::Result<()> {
    let host_raw = cli
        .remote
        .clone()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--remote requires a host"))?;
    if cli.has_startup_options() {
        eprintln!(
            "warning: startup options are ignored with --remote; sessions run on the remote host"
        );
    }

    let bridge = start_bridge(&host_raw)?;
    let result = client::run_attach_on_socket(&cli, bridge.socket_path());
    match result {
        Err(err) if !bridge.saw_remote_bytes() => Err(io::Error::new(
            err.kind(),
            format!(
                "{err}; no data received from remote host — check that ssh can reach '{}' and that spectra is installed there (on PATH or at ~/.local/bin)",
                bridge.host()
            ),
        )),
        other => other,
    }
}

/// Entry point for the hidden `remote-client-bridge` subcommand on the remote
/// host: stdin -> client socket and client socket -> stdout, until either
/// side closes.
pub fn run_bridge(cli: &Cli) -> io::Result<()> {
    let stream = client::connect_or_spawn_stream(cli)?;
    let mut socket_reader = stream.try_clone()?;
    let socket_writer = stream;

    let _stdin_pump = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut writer = &socket_writer;
        if let Err(err) = pump(&mut stdin, &mut writer, None) {
            eprintln!("spectra remote-client-bridge: stdin relay ended: {err}");
        }
        // Signal EOF to the server so it drops this client.
        let _ = socket_writer.shutdown(Shutdown::Write);
    });

    let mut stdout = io::stdout().lock();
    let result = pump(&mut socket_reader, &mut stdout, None);
    // Unblock the stdin pump's socket writes; the process exits right after,
    // which also releases the thread if it is still blocked reading stdin.
    let _ = socket_reader.shutdown(Shutdown::Both);
    result
}

/// A running `--remote` bridge: private 0700 dir + Unix listener whose
/// connections are relayed through the ssh transport. Dropping it stops the
/// accept loop and removes the directory.
pub struct RemoteBridge {
    host: String,
    dir: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    saw_remote_bytes: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl RemoteBridge {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Whether any bytes ever came back from the remote side (used to tell
    /// transport failures apart from ordinary detach/server errors).
    pub fn saw_remote_bytes(&self) -> bool {
        self.saw_remote_bytes.load(Ordering::SeqCst)
    }
}

impl Drop for RemoteBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Create the private bridge listener for `host` and start accepting
/// connections, spawning one ssh transport per connection.
pub fn start_bridge(host: &str) -> io::Result<RemoteBridge> {
    let host = normalize_host(host)?;
    let transport = transport_command(&host);

    let runtime_dir = socket_path::socket_path()
        .parent()
        .ok_or_else(|| io::Error::other("runtime socket path has no parent directory"))?
        .to_path_buf();
    let dir = runtime_dir.join(format!("remote-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

    let socket = dir.join("bridge.sock");
    let listener = bind_bridge_listener(&socket).inspect_err(|_err| {
        // Nothing owns the private dir yet; do not leak it on bind failure.
        let _ = fs::remove_dir_all(&dir);
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    let saw_remote_bytes = Arc::new(AtomicBool::new(false));
    let accept_thread = {
        let stop = Arc::clone(&stop);
        let saw_remote_bytes = Arc::clone(&saw_remote_bytes);
        thread::spawn(move || accept_loop(listener, transport, stop, saw_remote_bytes))
    };

    Ok(RemoteBridge {
        host,
        dir,
        socket,
        stop,
        saw_remote_bytes,
        accept_thread: Some(accept_thread),
    })
}

fn bind_bridge_listener(socket: &Path) -> io::Result<UnixListener> {
    socket_path::prepare_listener_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn accept_loop(
    listener: UnixListener,
    transport: Vec<String>,
    stop: Arc<AtomicBool>,
    saw_remote_bytes: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((conn, _addr)) => {
                if conn.set_nonblocking(false).is_err() {
                    continue;
                }
                let transport = transport.clone();
                let saw_remote_bytes = Arc::clone(&saw_remote_bytes);
                thread::spawn(move || {
                    if let Err(err) = relay_connection(conn, &transport, &saw_remote_bytes) {
                        eprintln!("spectra: remote bridge connection ended: {err}");
                    }
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("spectra: remote bridge listener failed: {err}");
                break;
            }
        }
    }
}

/// Pump one accepted local connection through a freshly spawned ssh
/// transport. ssh stderr is inherited so auth prompts and errors reach the
/// local terminal's stderr.
fn relay_connection(
    conn: UnixStream,
    transport: &[String],
    saw_remote_bytes: &Arc<AtomicBool>,
) -> io::Result<()> {
    let (program, args) = transport
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty transport command"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut ssh_stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("ssh transport stdin unavailable"))?;
    let mut ssh_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("ssh transport stdout unavailable"))?;

    let mut conn_reader = conn.try_clone()?;
    let to_remote = thread::spawn(move || {
        let _ = pump(&mut conn_reader, &mut ssh_stdin, None);
        // Dropping ssh stdin sends EOF to the remote bridge.
    });

    let result = {
        let mut writer = &conn;
        pump(&mut ssh_stdout, &mut writer, Some(saw_remote_bytes))
    };
    // The remote side is gone; closing both halves unblocks the other pump.
    let _ = conn.shutdown(Shutdown::Both);
    let _ = to_remote.join();
    let _ = child.wait();
    result
}

/// Copy bytes until EOF. `saw_bytes` (if provided) is flipped as soon as any
/// data arrives.
fn pump<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    saw_bytes: Option<&AtomicBool>,
) -> io::Result<()> {
    let mut chunk = [0u8; RELAY_CHUNK_BYTES];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if let Some(flag) = saw_bytes {
                    flag.store(true, Ordering::SeqCst);
                }
                writer.write_all(&chunk[..n])?;
                writer.flush()?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn normalize_host(raw: &str) -> io::Result<String> {
    let mut host = raw.trim();
    if let Some(stripped) = host.strip_prefix("ssh://") {
        host = stripped;
    }
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--remote host cannot be empty",
        ));
    }
    if host.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --remote host: {host}"),
        ));
    }
    Ok(host.to_string())
}

fn transport_command(host: &str) -> Vec<String> {
    let override_cmd = std::env::var(REMOTE_SSH_CMD_ENV).ok();
    transport_command_with_override(host, override_cmd.as_deref())
}

/// The transport argv: prefix (default `ssh -T -- <host>`, overridable via
/// [`REMOTE_SSH_CMD_ENV`]) plus the composed remote command as one final
/// argument. ssh joins remote command words with spaces, so the `sh -lc`
/// payload must stay quoted inside a single argument.
fn transport_command_with_override(host: &str, override_cmd: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = match override_cmd {
        Some(cmd) if !cmd.trim().is_empty() => cmd.split_whitespace().map(str::to_string).collect(),
        _ => vec![
            "ssh".to_string(),
            "-T".to_string(),
            "--".to_string(),
            host.to_string(),
        ],
    };
    argv.push(format!("sh -lc '{REMOTE_BRIDGE_COMMAND}'"));
    argv
}

#[cfg(test)]
mod tests {
    use super::{REMOTE_BRIDGE_COMMAND, normalize_host, transport_command_with_override};

    #[test]
    fn normalize_host_accepts_plain_and_user_at_host() {
        assert_eq!(normalize_host("box").expect("plain"), "box");
        assert_eq!(normalize_host("me@box").expect("user@host"), "me@box");
    }

    #[test]
    fn normalize_host_strips_ssh_scheme() {
        assert_eq!(normalize_host("ssh://me@box").expect("scheme"), "me@box");
        assert_eq!(
            normalize_host("ssh://me@box/").expect("scheme with slash"),
            "me@box"
        );
    }

    #[test]
    fn normalize_host_rejects_empty_and_option_like_hosts() {
        assert!(normalize_host("").is_err());
        assert!(normalize_host("ssh://").is_err());
        assert!(normalize_host("-oProxyCommand=evil").is_err());
    }

    #[test]
    fn transport_defaults_to_ssh_with_quoted_remote_command() {
        let argv = transport_command_with_override("me@box", None);
        assert_eq!(argv[..4], ["ssh", "-T", "--", "me@box"]);
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[4], format!("sh -lc '{REMOTE_BRIDGE_COMMAND}'"));
    }

    #[test]
    fn transport_override_replaces_prefix_and_keeps_remote_command() {
        let argv = transport_command_with_override("me@box", Some("env A=b sh -c"));
        assert_eq!(argv[..4], ["env", "A=b", "sh", "-c"]);
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[4], format!("sh -lc '{REMOTE_BRIDGE_COMMAND}'"));
    }

    #[test]
    fn transport_blank_override_falls_back_to_ssh() {
        let argv = transport_command_with_override("box", Some("   "));
        assert_eq!(argv[..4], ["ssh", "-T", "--", "box"]);
    }

    #[test]
    fn remote_bridge_command_survives_single_quoting() {
        // The remote command is wrapped in single quotes for ssh; it must not
        // contain one itself.
        assert!(!REMOTE_BRIDGE_COMMAND.contains('\''));
    }
}
