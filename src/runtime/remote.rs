#![cfg(unix)]

//! Simplified remote attach over an ssh stdio bridge.
//!
//! Local side (`spectra --remote <host>`): the local binary is first seeded
//! to the remote host (probe `uname` + sha256 of the previously seeded copy
//! over ssh; stream the binary over ssh stdin when missing or different),
//! then a private per-invocation Unix listener socket is created under the
//! runtime dir, the normal interactive client attaches to it, and every
//! accepted connection is pumped through `ssh -T` stdin/stdout to the remote
//! host, executing the seeded binary. When the remote platform differs from
//! the local one (so the local binary cannot run there), the remote host
//! instead downloads the matching release asset for its own platform from
//! GitHub — same version as the local build — into the seeded path. Only
//! when that also fails (unreleased dev version, unsupported platform, no
//! curl/wget) does the bridge fall back to a spectra already installed on
//! the remote host.
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

/// Path relative to `$HOME` on the remote host where `--remote` seeds a copy
/// of the local binary before executing it.
pub const REMOTE_SEEDED_BINARY_SUFFIX: &str = ".local/share/spectra/bin/spectra";

/// Fallback bridge command through `sh -lc` (login shell so PATH
/// customizations like `~/.local/bin` resolve), used only when the remote
/// platform differs from the local one and the local binary cannot be
/// seeded. Falls back to the default install location when `spectra` is not
/// on PATH.
pub const REMOTE_BRIDGE_COMMAND: &str = "if command -v spectra >/dev/null 2>&1; then exec spectra remote-client-bridge; else exec \"$HOME/.local/bin/spectra\" remote-client-bridge; fi";

/// Test seam: when set, its whitespace-split value replaces the default
/// `ssh -T -- <host>` transport prefix. The composed `sh -lc '...'` remote
/// command is still appended as the final argument.
pub const REMOTE_SSH_CMD_ENV: &str = "SPECTRA_REMOTE_SSH_CMD";

/// When set, seed this binary to the remote host instead of the running
/// executable (e.g. a build cross-compiled for the remote platform; also the
/// test seam, where the running executable is the test harness).
pub const REMOTE_BINARY_ENV: &str = "SPECTRA_REMOTE_BINARY";

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
                "{err}; no data received from remote host — check that ssh can reach '{}'",
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
/// connections, spawning one ssh transport per connection. Seeds the local
/// binary to the remote host first so the bridge runs exactly this build.
pub fn start_bridge(host: &str) -> io::Result<RemoteBridge> {
    let host = normalize_host(host)?;
    let prefix = transport_prefix(&host);
    let exec = ensure_remote_binary(&host, &prefix)?;
    let transport = compose_transport(&prefix, &exec.bridge_command());

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
    let (program, args) = split_transport(transport)?;
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

fn transport_prefix(host: &str) -> Vec<String> {
    let override_cmd = std::env::var(REMOTE_SSH_CMD_ENV).ok();
    transport_prefix_with_override(host, override_cmd.as_deref())
}

/// The transport argv prefix: default `ssh -T -- <host>`, overridable via
/// [`REMOTE_SSH_CMD_ENV`]. Remote commands are appended as one final
/// argument.
fn transport_prefix_with_override(host: &str, override_cmd: Option<&str>) -> Vec<String> {
    match override_cmd {
        Some(cmd) if !cmd.trim().is_empty() => cmd.split_whitespace().map(str::to_string).collect(),
        _ => vec![
            "ssh".to_string(),
            "-T".to_string(),
            "--".to_string(),
            host.to_string(),
        ],
    }
}

/// Compose the full per-connection transport argv. ssh joins remote command
/// words with spaces, so the `sh -lc` payload must stay quoted inside a
/// single argument (`remote_command` must not contain single quotes).
fn compose_transport(prefix: &[String], remote_command: &str) -> Vec<String> {
    let mut argv = prefix.to_vec();
    argv.push(format!("sh -lc '{remote_command}'"));
    argv
}

/// How the bridge is executed on the remote host.
enum RemoteExec {
    /// The binary at [`REMOTE_SEEDED_BINARY_SUFFIX`]: either a seeded copy of
    /// the local binary (same platform) or a release asset the remote host
    /// downloaded from GitHub for its own platform.
    Seeded,
    /// A spectra already installed on the remote host (platform mismatch and
    /// no release asset could be fetched).
    PathDiscovery,
}

impl RemoteExec {
    fn bridge_command(&self) -> String {
        match self {
            Self::Seeded => {
                format!("exec \"$HOME/{REMOTE_SEEDED_BINARY_SUFFIX}\" remote-client-bridge")
            }
            Self::PathDiscovery => REMOTE_BRIDGE_COMMAND.to_string(),
        }
    }
}

/// Shell script that reports the remote platform and the state of the
/// previously seeded binary, one item per line: `uname -s`, `uname -m`, then
/// the binary's sha256 hex (or `missing` / `hash-unavailable`). Runs via
/// `sh -c '...'` inside the remote user's shell, so it must not contain
/// single quotes.
fn probe_script() -> String {
    format!(
        r#"uname -s
uname -m
dest=$HOME/{REMOTE_SEEDED_BINARY_SUFFIX}
if [ -x "$dest" ]; then
  if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum <"$dest"); echo "${{h%% *}}"
  elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 <"$dest"); echo "${{h%% *}}"
  else echo hash-unavailable
  fi
else echo missing
fi"#
    )
}

/// Shell script that installs the binary streamed on stdin at the seeded
/// path (atomic tmp + mv). Runs via `sh -c '...'`, so no single quotes.
fn seed_script() -> String {
    format!(
        r#"dest=$HOME/{REMOTE_SEEDED_BINARY_SUFFIX}
dir=${{dest%/*}}
mkdir -p "$dir"
tmp=$dest.tmp.$$
cat >"$tmp"
chmod 755 "$tmp"
mv "$tmp" "$dest""#
    )
}

/// GitHub repository the remote host downloads release assets from when its
/// platform differs from the local one. Must match the release workflow's
/// asset naming: `spectra-v<version>-<target>.tar.gz`.
const RELEASE_REPO: &str = "aplio/spectra";

/// Release asset target suffix for a normalized remote `uname -s`/`-m` pair,
/// or `None` when no prebuilt asset exists for that platform.
fn release_target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64") => Some("linux-x86_64"),
        ("Linux", "aarch64") => Some("linux-aarch64"),
        ("Darwin", "aarch64") => Some("macos-arm64"),
        ("Darwin", "x86_64") => Some("macos-x86_64"),
        _ => None,
    }
}

/// Shell script that makes the remote host download the release asset for
/// its own platform from GitHub into the seeded path (atomic tmp + mv),
/// skipping the download when the seeded binary already reports `version`.
/// Runs via `sh -c '...'`, so no single quotes.
fn download_script(version: &str, target: &str) -> String {
    format!(
        r#"dest=$HOME/{REMOTE_SEEDED_BINARY_SUFFIX}
if [ -x "$dest" ] && [ "$("$dest" --version 2>/dev/null)" = "spectra {version}" ]; then exit 0; fi
url=https://github.com/{RELEASE_REPO}/releases/download/v{version}/spectra-v{version}-{target}.tar.gz
dir=${{dest%/*}}
mkdir -p "$dir"
tmp=$dir/.fetch.$$
mkdir -p "$tmp"
trap "rm -rf \"$tmp\"" EXIT
if command -v curl >/dev/null 2>&1; then curl -fsSL -o "$tmp/spectra.tar.gz" "$url"
elif command -v wget >/dev/null 2>&1; then wget -q -O "$tmp/spectra.tar.gz" "$url"
else echo "spectra: neither curl nor wget is available on the remote host" >&2; exit 3
fi || exit 4
tar -xzf "$tmp/spectra.tar.gz" -C "$tmp" spectra || exit 5
chmod 755 "$tmp/spectra"
mv "$tmp/spectra" "$dest""#
    )
}

/// Make sure the remote host has a runnable spectra: seed the local binary
/// when it is missing or differs (by sha256). When the platforms differ the
/// remote host downloads the release asset for its own platform from GitHub
/// instead, falling back to a remotely installed spectra when that fails.
fn ensure_remote_binary(host: &str, prefix: &[String]) -> io::Result<RemoteExec> {
    let probe = run_probe(host, prefix)?;
    let (local_os, local_arch) = local_platform();
    if probe.os != local_os || probe.arch != local_arch {
        return Ok(ensure_cross_platform_binary(host, prefix, &probe));
    }

    let local_exe = local_binary_path()?;
    if let SeededBinary::Hash(remote_hash) = &probe.seeded
        && *remote_hash == sha256_hex(&local_exe)?
    {
        return Ok(RemoteExec::Seeded);
    }

    eprintln!("spectra: seeding local binary to {host}:~/{REMOTE_SEEDED_BINARY_SUFFIX}");
    seed_remote_binary(prefix, &local_exe)?;
    Ok(RemoteExec::Seeded)
}

/// Platform mismatch path: the local binary cannot run on the remote host,
/// so have the remote host download the release asset for its own platform
/// (same version as the local build) from GitHub. Any failure falls back to
/// a spectra already installed on the remote host.
fn ensure_cross_platform_binary(host: &str, prefix: &[String], probe: &RemoteProbe) -> RemoteExec {
    let (local_os, local_arch) = local_platform();
    let Some(target) = release_target(&probe.os, &probe.arch) else {
        eprintln!(
            "spectra: remote platform {}/{} differs from local {}/{} and has no prebuilt release — using spectra installed on '{host}'",
            probe.os, probe.arch, local_os, local_arch
        );
        return RemoteExec::PathDiscovery;
    };

    let version = env!("CARGO_PKG_VERSION");
    eprintln!(
        "spectra: remote platform {}/{} differs from local {}/{}; fetching v{version} ({target}) from GitHub on '{host}'",
        probe.os, probe.arch, local_os, local_arch
    );
    match run_download(prefix, version, target) {
        Ok(()) => RemoteExec::Seeded,
        Err(err) => {
            eprintln!(
                "spectra: downloading the release binary on '{host}' failed ({err}) — using spectra installed there instead"
            );
            RemoteExec::PathDiscovery
        }
    }
}

/// Run the download script on the remote host over the transport.
fn run_download(prefix: &[String], version: &str, target: &str) -> io::Result<()> {
    let (program, args) = split_transport(prefix)?;
    let status = Command::new(program)
        .args(args)
        .arg(format!("sh -c '{}'", download_script(version, target)))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "download script exited with {status}"
        )))
    }
}

struct RemoteProbe {
    os: String,
    arch: String,
    seeded: SeededBinary,
}

enum SeededBinary {
    Hash(String),
    /// Missing, or present but the remote host has no sha256 tool to compare
    /// it with — either way the binary gets (re)seeded.
    Unknown,
}

fn run_probe(host: &str, prefix: &[String]) -> io::Result<RemoteProbe> {
    let (program, args) = split_transport(prefix)?;
    let output = Command::new(program)
        .args(args)
        .arg(format!("sh -c '{}'", probe_script()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "remote probe on '{host}' exited with {} — check that ssh can reach it",
            output.status
        )));
    }
    parse_probe_output(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected probe output from '{host}'"),
        )
    })
}

fn parse_probe_output(stdout: &str) -> Option<RemoteProbe> {
    let mut lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let os = lines.next()?.to_string();
    let arch = normalize_arch(lines.next()?);
    let seeded = match lines.next()? {
        "missing" | "hash-unavailable" => SeededBinary::Unknown,
        hash if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            SeededBinary::Hash(hash.to_string())
        }
        _ => return None,
    };
    Some(RemoteProbe { os, arch, seeded })
}

/// `uname -s` / `uname -m` values for the local build, normalized the same
/// way as the probe output so they compare directly.
fn local_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "Darwin"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    (os, arch)
}

fn normalize_arch(arch: &str) -> String {
    match arch {
        "arm64" => "aarch64".to_string(),
        "amd64" => "x86_64".to_string(),
        other => other.to_string(),
    }
}

fn local_binary_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(REMOTE_BINARY_ENV) {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe()
}

fn sha256_hex(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Stream the local binary over the transport's stdin into the seeded path.
fn seed_remote_binary(prefix: &[String], local_exe: &Path) -> io::Result<()> {
    let (program, args) = split_transport(prefix)?;
    let mut child = Command::new(program)
        .args(args)
        .arg(format!("sh -c '{}'", seed_script()))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("seed transport stdin unavailable"))?;
    let mut source = fs::File::open(local_exe)?;
    let copy_result = io::copy(&mut source, &mut stdin).map(|_| ());
    // Dropping stdin sends EOF so the remote `cat` finishes.
    drop(stdin);
    let status = child.wait()?;
    copy_result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "seeding the remote binary exited with {status}"
        )))
    }
}

fn split_transport(transport: &[String]) -> io::Result<(&String, &[String])> {
    transport
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty transport command"))
}

#[cfg(test)]
mod tests {
    use super::{
        REMOTE_BRIDGE_COMMAND, REMOTE_SEEDED_BINARY_SUFFIX, RemoteExec, SeededBinary,
        compose_transport, download_script, normalize_host, parse_probe_output, probe_script,
        release_target, seed_script, transport_prefix_with_override,
    };

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
        let prefix = transport_prefix_with_override("me@box", None);
        assert_eq!(prefix, ["ssh", "-T", "--", "me@box"]);
        let argv = compose_transport(&prefix, &RemoteExec::Seeded.bridge_command());
        assert_eq!(argv.len(), 5);
        assert_eq!(
            argv[4],
            format!("sh -lc 'exec \"$HOME/{REMOTE_SEEDED_BINARY_SUFFIX}\" remote-client-bridge'")
        );
    }

    #[test]
    fn transport_override_replaces_prefix_and_keeps_remote_command() {
        let prefix = transport_prefix_with_override("me@box", Some("env A=b sh -c"));
        assert_eq!(prefix, ["env", "A=b", "sh", "-c"]);
        let argv = compose_transport(&prefix, &RemoteExec::PathDiscovery.bridge_command());
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[4], format!("sh -lc '{REMOTE_BRIDGE_COMMAND}'"));
    }

    #[test]
    fn transport_blank_override_falls_back_to_ssh() {
        let prefix = transport_prefix_with_override("box", Some("   "));
        assert_eq!(prefix, ["ssh", "-T", "--", "box"]);
    }

    #[test]
    fn remote_commands_survive_single_quoting() {
        // Remote commands are wrapped in single quotes (for ssh and for the
        // remote user's shell via `sh -c '...'`); they must not contain one
        // themselves.
        assert!(!REMOTE_BRIDGE_COMMAND.contains('\''));
        assert!(!RemoteExec::Seeded.bridge_command().contains('\''));
        assert!(!probe_script().contains('\''));
        assert!(!seed_script().contains('\''));
        assert!(!download_script("0.2.15", "linux-aarch64").contains('\''));
    }

    #[test]
    fn release_target_maps_supported_platforms() {
        assert_eq!(release_target("Linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(release_target("Linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(release_target("Darwin", "aarch64"), Some("macos-arm64"));
        assert_eq!(release_target("Darwin", "x86_64"), Some("macos-x86_64"));
        assert_eq!(release_target("FreeBSD", "x86_64"), None);
        assert_eq!(release_target("Linux", "riscv64"), None);
    }

    #[test]
    fn download_script_targets_versioned_release_asset() {
        let script = download_script("0.2.15", "linux-aarch64");
        assert!(script.contains(
            "https://github.com/aplio/spectra/releases/download/v0.2.15/spectra-v0.2.15-linux-aarch64.tar.gz"
        ));
        // Re-running is a no-op when the seeded binary already matches.
        assert!(script.contains("= \"spectra 0.2.15\""));
    }

    #[test]
    fn probe_output_parses_platform_and_hash() {
        let probe = parse_probe_output(
            "Linux\nx86_64\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .expect("valid probe output");
        assert_eq!(probe.os, "Linux");
        assert_eq!(probe.arch, "x86_64");
        assert!(matches!(probe.seeded, SeededBinary::Hash(hash) if hash.len() == 64));
    }

    #[test]
    fn probe_output_normalizes_arch_and_handles_missing_binary() {
        let probe = parse_probe_output("Darwin\narm64\nmissing\n").expect("valid probe output");
        assert_eq!(probe.os, "Darwin");
        assert_eq!(probe.arch, "aarch64");
        assert!(matches!(probe.seeded, SeededBinary::Unknown));

        let probe =
            parse_probe_output("Linux\namd64\nhash-unavailable\n").expect("valid probe output");
        assert_eq!(probe.arch, "x86_64");
        assert!(matches!(probe.seeded, SeededBinary::Unknown));
    }

    #[test]
    fn probe_output_rejects_garbage() {
        assert!(parse_probe_output("").is_none());
        assert!(parse_probe_output("Linux\nx86_64\nnot-a-hash\n").is_none());
        assert!(parse_probe_output("Linux\n").is_none());
    }
}
