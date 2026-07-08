//! Live server handoff, successor side (`spectra server-handoff`).
//!
//! Protocol (v1) over `spectra-handoff.sock`, bound by the OUTGOING server
//! only for the duration of one handoff:
//!
//! 1. successor → old API socket: `server.handoff` JSON-RPC request; the
//!    old server validates (refused while any client is attached, or with
//!    more panes than the fd cap) and answers with the handoff socket path.
//! 2. successor connects to the handoff socket; old server sends one JSON
//!    header line (runtime state + per-pane fd-index/replay metadata).
//! 3. successor acks the header; old server sends the PTY master fds as
//!    SCM_RIGHTS batches of ≤32 fds per message.
//! 4. successor acks the fds — the point of no return: the old server
//!    disarms kill-on-drop for every pane child, unlinks its listener
//!    sockets, sends the completion line, and exits.
//! 5. successor binds fresh listener sockets, rebuilds the `App` around the
//!    adopted fds, replays each pane's retained output tail
//!    (`[pane] handoff_replay_bytes`), and runs the normal server loop.
//!
//! Any failure before step 4 leaves the old server fully functional — it
//! logs the abort and keeps serving; nothing is disarmed or unbound.
//!
//! v1 limitation: handoff is refused while clients are attached (there is
//! no client auto-reconnect yet). Panes and their processes survive; users
//! reattach manually.

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::app::handoff::{HANDOFF_VERSION, HandoffHeader, MAX_FDS_PER_HANDOFF};
use crate::cli::{Cli, CliCommand};
use crate::ipc::fdpass;
use crate::ipc::socket_path;

/// Successor → old server: header line parsed successfully.
pub(crate) const HEADER_ACK: &str = "spectra-handoff-header-ok";
/// Successor → old server: all fds received (point of no return).
pub(crate) const FDS_ACK: &str = "spectra-handoff-fds-ok";
/// Old server → successor: children disarmed, listener sockets unlinked.
pub(crate) const COMPLETE: &str = "spectra-handoff-complete";

/// Status line the foreground takeover prints to stdout for the
/// coordinator: `spectra-handoff-ok <pane_count>` on success.
const STATUS_OK_PREFIX: &str = "spectra-handoff-ok";
/// Status line prefix for takeover failures.
const STATUS_ERROR_PREFIX: &str = "spectra-handoff-error:";

/// Longest one protocol step may take before the exchange is aborted.
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the coordinator waits for the detached takeover to report.
const COORDINATOR_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on the JSON header line (guards a byte-wise line reader).
const MAX_HEADER_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Post-update hook: run the live handoff through the freshly installed
/// binary (the calling process still executes the old code, so it must not
/// perform the takeover itself). The server-side pre-flight refuses while
/// clients are attached, so this succeeds exactly when a handoff is safe;
/// any failure leaves the old server fully serving.
pub fn run_post_update_handoff(installed_exe: Option<&std::path::Path>) -> io::Result<()> {
    let exe = installed_exe.ok_or_else(|| {
        io::Error::other("could not determine the installed binary path for the live handoff")
    })?;
    println!("Attempting live handoff to move the running server onto the new binary...");
    let status = Command::new(exe)
        .arg("server-handoff")
        .stdin(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "the live handoff did not complete (see the message above)",
        ))
    }
}

pub fn run(cli: Cli) -> io::Result<()> {
    let foreground = match &cli.subcommand {
        Some(CliCommand::ServerHandoff { foreground }) => *foreground,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing server-handoff subcommand",
            ));
        }
    };
    if foreground {
        run_foreground_takeover(cli)
    } else {
        run_coordinator()
    }
}

/// User-facing entry point: spawn the actual takeover as a detached child
/// (which becomes the new server, so it must not stay tied to this
/// terminal's lifetime) and relay its one-line status report.
fn run_coordinator() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .args(["server-handoff", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture takeover process stdout"))?;
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = io::BufReader::new(stdout);
        let mut line = String::new();
        if io::BufRead::read_line(&mut reader, &mut line).is_ok() {
            let _ = tx.send(line);
        }
    });

    match rx.recv_timeout(COORDINATOR_TIMEOUT) {
        Ok(line) => {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(STATUS_OK_PREFIX) {
                let pane_count = rest.trim();
                println!(
                    "server handoff complete: {pane_count} pane(s) adopted by the new server (pid {}); reattach with `spectra`",
                    child.id()
                );
                // The child keeps running as the new server; do not wait on it.
                Ok(())
            } else if let Some(message) = line.strip_prefix(STATUS_ERROR_PREFIX) {
                let _ = child.wait();
                Err(io::Error::other(message.trim().to_string()))
            } else {
                let _ = child.kill();
                let _ = child.wait();
                Err(io::Error::other(format!(
                    "unexpected takeover status line: {line:?}"
                )))
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for the server handoff to complete",
            ))
        }
    }
}

/// Perform the takeover in this process and become the new server.
fn run_foreground_takeover(cli: Cli) -> io::Result<()> {
    let takeover = match take_over_running_server() {
        Ok(takeover) => takeover,
        Err(err) => {
            println!("{STATUS_ERROR_PREFIX} {err}");
            let _ = io::stdout().flush();
            return Err(err);
        }
    };
    crate::runtime::server::run_adopted(cli, takeover)
}

/// Everything received from the outgoing server.
pub(crate) struct HandoffTakeover {
    pub header: HandoffHeader,
    pub fds: Vec<OwnedFd>,
}

/// Full successor-side exchange: request the handoff, receive header and
/// fds, ack, and wait for the old server to release its sockets.
fn take_over_running_server() -> io::Result<HandoffTakeover> {
    let handoff_socket = request_handoff_via_api()?;

    let stream = connect_with_retry(&handoff_socket, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(EXCHANGE_TIMEOUT))?;
    stream.set_write_timeout(Some(EXCHANGE_TIMEOUT))?;

    let header_line = read_line(&stream, MAX_HEADER_LINE_BYTES)?;
    let header: HandoffHeader = serde_json::from_str(&header_line).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid handoff header: {err}"),
        )
    })?;
    if header.version() != HANDOFF_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff version mismatch (old server {}, this binary {HANDOFF_VERSION})",
                header.version()
            ),
        ));
    }
    if header.fd_count() > MAX_FDS_PER_HANDOFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff declares {} fds, above the cap of {MAX_FDS_PER_HANDOFF}",
                header.fd_count()
            ),
        ));
    }
    write_line(&stream, HEADER_ACK)?;

    let mut fds: Vec<OwnedFd> = Vec::with_capacity(header.fd_count());
    while fds.len() < header.fd_count() {
        let mut payload = [0u8; 16];
        let (n, batch) = fdpass::recv_with_fds(&stream, &mut payload)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "old server closed the handoff socket after {} of {} fds",
                    fds.len(),
                    header.fd_count()
                ),
            ));
        }
        fds.extend(batch);
        if fds.len() > header.fd_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "received {} fds but the header declared {}",
                    fds.len(),
                    header.fd_count()
                ),
            ));
        }
    }
    write_line(&stream, FDS_ACK)?;

    let complete = read_line(&stream, 4096)?;
    if complete.trim() != COMPLETE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected handoff completion, got {complete:?}"),
        ));
    }

    Ok(HandoffTakeover { header, fds })
}

/// Call `server.handoff` on the running server's API socket and return the
/// handoff socket path from the response.
fn request_handoff_via_api() -> io::Result<std::path::PathBuf> {
    let api_socket = socket_path::api_socket_path();
    let stream = UnixStream::connect(&api_socket).map_err(|err| {
        io::Error::new(
            err.kind(),
            "no spectra server is running (nothing to hand off)",
        )
    })?;
    stream.set_read_timeout(Some(EXCHANGE_TIMEOUT))?;
    stream.set_write_timeout(Some(EXCHANGE_TIMEOUT))?;

    write_line(&stream, r#"{"id":1,"method":"server.handoff"}"#)?;
    let line = read_line(&stream, 1024 * 1024)?;
    let response: serde_json::Value = serde_json::from_str(line.trim()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid server.handoff response: {err}"),
        )
    })?;

    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(io::Error::other(message));
    }
    let socket = response
        .get("result")
        .and_then(|result| result.get("socket"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("server.handoff response has no socket path: {response}"),
            )
        })?;
    let version = response
        .get("result")
        .and_then(|result| result.get("version"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version != u64::from(HANDOFF_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff protocol mismatch (old server v{version}, this binary v{HANDOFF_VERSION})"
            ),
        ));
    }
    Ok(std::path::PathBuf::from(socket))
}

fn connect_with_retry(path: &std::path::Path, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "timed out connecting to handoff socket {}: {err}",
                            path.display()
                        ),
                    ));
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Announce a successful takeover on stdout (one line the coordinator
/// waits for), then detach stdout so nothing else can hit a closed pipe.
pub(crate) fn announce_takeover_ready(pane_count: usize) {
    println!("{STATUS_OK_PREFIX} {pane_count}");
    let _ = io::stdout().flush();
    if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: redirecting our own stdout to an fd we hold open.
        unsafe {
            libc::dup2(devnull.as_raw_fd(), libc::STDOUT_FILENO);
        }
    }
}

/// Read one `\n`-terminated line byte-by-byte. Byte-wise reads are
/// deliberate on the handoff socket: over-reading past the header line
/// with a buffered reader would consume payload bytes that carry the
/// SCM_RIGHTS ancillary data and silently drop the attached fds.
pub(crate) fn read_line(mut stream: &UnixStream, max_bytes: usize) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed while reading a protocol line",
                ));
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return String::from_utf8(line).map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("protocol line is not utf-8: {err}"),
                        )
                    });
                }
                line.push(byte[0]);
                if line.len() > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("protocol line exceeds {max_bytes} bytes"),
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn write_line(mut stream: &UnixStream, line: &str) -> io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::{read_line, write_line};

    #[test]
    fn line_helpers_roundtrip_over_a_socketpair() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        write_line(&a, "hello-protocol").expect("write line");
        let line = read_line(&b, 1024).expect("read line");
        assert_eq!(line, "hello-protocol");
    }

    #[test]
    fn read_line_enforces_the_length_cap() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        write_line(&a, &"x".repeat(64)).expect("write long line");
        let err = read_line(&b, 16).expect_err("line over cap");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_reports_eof() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        drop(a);
        let err = read_line(&b, 16).expect_err("eof");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
