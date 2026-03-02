#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEventKind};

use crate::attach_target::AttachTarget;
use crate::cli::{Cli, CliCommand, CliMode};
use crate::io::terminal;
use crate::ipc::codec::{decode_messages, encode_message};
use crate::ipc::protocol::{
    ClientMessage, CommandRequest, CommandResult, CommandSplitAxis, NetKeyEvent, NetMouseEvent,
    ServerMessage,
};
use crate::ipc::socket_path;
use crate::core_lib::runtime::event_loop::poll_event_for;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const IDLE_LOOP_BACKOFF: Duration = Duration::from_millis(1);
const SPECTRA_NESTED_WARNING: &str = "sessions should be nested with care, unset $SPECTRA to force";

pub fn nested_session_warning(mode: CliMode) -> Option<&'static str> {
    if mode != CliMode::AttachOrCreate {
        return None;
    }
    if !inside_spectra_session() {
        return None;
    }
    Some(SPECTRA_NESTED_WARNING)
}

struct ConnectedStream {
    stream: UnixStream,
    attached_existing: bool,
}

pub fn run_attach_or_create(cli: Cli) -> io::Result<()> {
    let attach_target = parse_attach_target(cli.attach_target_raw(), "--attach")?;
    let connected = connect_or_spawn(&cli)?;

    if connected.attached_existing && cli.has_startup_options() {
        eprintln!(
            "warning: startup options are ignored while attaching to an existing server session"
        );
    }

    run_client(connected.stream, attach_target)
}

pub fn run_command(cli: Cli) -> io::Result<()> {
    let subcommand = cli
        .subcommand
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing command subcommand"))?;
    let request = command_request_from_cli(subcommand)?;

    let mut connected = connect_or_spawn(&cli)?;
    if connected.attached_existing && cli.has_startup_options() {
        eprintln!(
            "warning: startup options are ignored while running commands against an existing server"
        );
    }

    if !connected.attached_existing && matches!(request, CommandRequest::NewSession) {
        println!("session created");
        return Ok(());
    }

    let result = run_command_request(&mut connected.stream, request)?;
    print_command_result(result);
    Ok(())
}

fn run_client(mut stream: UnixStream, attach_target: Option<AttachTarget>) -> io::Result<()> {
    let mut stdout = terminal::setup();
    let result = run_client_loop(&mut stream, &mut stdout, attach_target);
    terminal::teardown(stdout);
    result
}

fn run_client_loop(
    stream: &mut UnixStream,
    stdout: &mut std::io::Stdout,
    attach_target: Option<AttachTarget>,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    send_client_message(
        stream,
        &ClientMessage::Hello {
            cols,
            rows,
            attach_target,
            client_identity: client_identity_fingerprint(),
        },
    )?;

    let mut read_buffer = Vec::new();
    loop {
        let mut did_work = false;

        while let Some(event) = poll_event_for(Duration::ZERO)? {
            did_work = true;
            match event {
                Event::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Release) {
                        continue;
                    }
                    send_client_message(
                        stream,
                        &ClientMessage::Key {
                            key: NetKeyEvent::from(key),
                        },
                    )?;
                }
                Event::Paste(text) => {
                    send_client_message(stream, &ClientMessage::Paste { text })?;
                }
                Event::Mouse(mouse) => {
                    send_client_message(
                        stream,
                        &ClientMessage::Mouse {
                            mouse: NetMouseEvent::from(mouse),
                        },
                    )?;
                }
                Event::Resize(cols, rows) => {
                    send_client_message(stream, &ClientMessage::Resize { cols, rows })?;
                }
                _ => {}
            }
        }

        let mut chunk = [0u8; 16 * 1024];
        let mut server_closed = false;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    server_closed = true;
                    break;
                }
                Ok(n) => {
                    did_work = true;
                    read_buffer.extend_from_slice(&chunk[..n]);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }

        let decoded = decode_messages::<ServerMessage>(&mut read_buffer);
        if let Some(err) = decoded.errors.first() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid server frame: {err}"),
            ));
        }
        let mut wrote_to_stdout = false;
        for message in decoded.messages {
            match message {
                ServerMessage::Render { ansi } => {
                    stdout.write_all(ansi.as_bytes())?;
                    wrote_to_stdout = true;
                }
                ServerMessage::Clipboard { ansi } => {
                    stdout.write_all(ansi.as_bytes())?;
                    wrote_to_stdout = true;
                }
                ServerMessage::Passthrough { ansi } => {
                    stdout.write_all(ansi.as_bytes())?;
                    wrote_to_stdout = true;
                }
                ServerMessage::Detached { .. } => {
                    if wrote_to_stdout {
                        stdout.flush()?;
                    }
                    return Ok(());
                }
                ServerMessage::Shutdown { reason } => {
                    if wrote_to_stdout {
                        stdout.flush()?;
                    }
                    return Err(io::Error::other(format!("server shutdown: {reason}")));
                }
                ServerMessage::Error { message } => {
                    if wrote_to_stdout {
                        stdout.flush()?;
                    }
                    return Err(io::Error::other(format!("server error: {message}")));
                }
                ServerMessage::CommandResult { .. } => {}
            }
        }
        if wrote_to_stdout {
            stdout.flush()?;
            did_work = true;
        }

        if server_closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "spectra server disconnected",
            ));
        }

        if !did_work {
            thread::sleep(IDLE_LOOP_BACKOFF);
        }
    }
}

fn run_command_request(
    stream: &mut UnixStream,
    request: CommandRequest,
) -> io::Result<CommandResult> {
    stream.set_nonblocking(true)?;
    send_client_message(stream, &ClientMessage::Command { request })?;

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut read_buffer = Vec::new();

    loop {
        let mut chunk = [0u8; 16 * 1024];
        let mut server_closed = false;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    server_closed = true;
                    break;
                }
                Ok(n) => {
                    read_buffer.extend_from_slice(&chunk[..n]);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }

        let decoded = decode_messages::<ServerMessage>(&mut read_buffer);
        if let Some(err) = decoded.errors.first() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid server frame: {err}"),
            ));
        }

        for message in decoded.messages {
            match message {
                ServerMessage::CommandResult { result } => return Ok(result),
                ServerMessage::Error { message } => {
                    return Err(io::Error::other(format!("server error: {message}")));
                }
                ServerMessage::Shutdown { reason } => {
                    return Err(io::Error::other(format!("server shutdown: {reason}")));
                }
                ServerMessage::Render { .. }
                | ServerMessage::Passthrough { .. }
                | ServerMessage::Clipboard { .. }
                | ServerMessage::Detached { .. } => {}
            }
        }

        if server_closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "spectra server disconnected",
            ));
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for command response",
            ));
        }
        thread::sleep(IO_RETRY_DELAY);
    }
}

fn print_command_result(result: CommandResult) {
    match result {
        CommandResult::Message { message } => {
            println!("{message}");
        }
        CommandResult::SessionList { sessions } => {
            for session in sessions {
                let active = if session.active { '*' } else { '-' };
                let focused_window = session.focused_window.unwrap_or(0);
                let focused_pane = session.focused_pane.unwrap_or(0);
                println!(
                    "{active} {} {} windows={} panes={} focus=w{}.p{} name={}",
                    session.alias,
                    session.session_id,
                    session.window_count,
                    session.pane_count,
                    focused_window,
                    focused_pane,
                    session.session_name
                );
            }
        }
    }
}

fn command_request_from_cli(command: &CliCommand) -> io::Result<CommandRequest> {
    match command {
        CliCommand::AttachSession { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attach-session is interactive and not a one-shot command",
        )),
        CliCommand::NewSession => Ok(CommandRequest::NewSession),
        CliCommand::Ls => Ok(CommandRequest::Ls),
        CliCommand::KillSession { target } => Ok(CommandRequest::KillSession {
            target: target.clone(),
        }),
        CliCommand::NewWindow { target } => Ok(CommandRequest::NewWindow {
            target: parse_attach_target(target.as_deref(), "--target")?,
        }),
        CliCommand::SplitWindow {
            horizontal,
            vertical: _vertical,
            target,
        } => {
            let axis = if *horizontal {
                CommandSplitAxis::Horizontal
            } else {
                CommandSplitAxis::Vertical
            };
            Ok(CommandRequest::SplitWindow {
                target: parse_attach_target(target.as_deref(), "--target")?,
                axis,
            })
        }
        CliCommand::SelectSession { target } => Ok(CommandRequest::SelectSession {
            target: target.clone(),
        }),
        CliCommand::SelectWindow { window, target } => {
            if *window == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "window must be >= 1",
                ));
            }
            Ok(CommandRequest::SelectWindow {
                target: target.clone(),
                window: *window,
            })
        }
        CliCommand::SelectPane { pane, target } => {
            if *pane == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pane must be >= 1",
                ));
            }
            Ok(CommandRequest::SelectPane {
                target: target.clone(),
                pane: *pane,
            })
        }
        CliCommand::SendKeys { target, all, text } => {
            let text = text.join(" ");
            if text.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "send-keys text cannot be empty",
                ));
            }
            Ok(CommandRequest::SendKeys {
                target: parse_attach_target(target.as_deref(), "--target")?,
                all: *all,
                text,
            })
        }
        CliCommand::SourceFile { path } => Ok(CommandRequest::SourceFile {
            path: path.as_ref().map(|path| path.display().to_string()),
        }),
    }
}

fn connect_or_spawn(cli: &Cli) -> io::Result<ConnectedStream> {
    let socket = socket_path::socket_path();
    match UnixStream::connect(&socket) {
        Ok(stream) => Ok(ConnectedStream {
            stream,
            attached_existing: true,
        }),
        Err(err) if should_spawn_server(&err) => {
            let stream = spawn_server_and_connect(cli, &socket)?;
            Ok(ConnectedStream {
                stream,
                attached_existing: false,
            })
        }
        Err(err) => Err(err),
    }
}

fn should_spawn_server(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::AddrNotAvailable
    )
}

fn spawn_server_and_connect(cli: &Cli, socket: &Path) -> io::Result<UnixStream> {
    let mut child = spawn_server_process(cli)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if let Some(status) = child.try_wait()? {
                    return Err(io::Error::other(format!(
                        "spectra server exited before attach: {status}"
                    )));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "timed out waiting for spectra server socket: {}",
                            socket.display()
                        ),
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn spawn_server_process(cli: &Cli) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command.arg("--server");
    if let Some(cwd) = &cli.cwd {
        command.arg("--cwd").arg(cwd);
    }
    if let Some(shell) = &cli.shell {
        command.arg("--shell").arg(shell);
    }
    if !cli.command.is_empty() {
        command.arg("--");
        command.args(&cli.command);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn()
}

fn send_client_message(stream: &mut UnixStream, message: &ClientMessage) -> io::Result<()> {
    let encoded = encode_message(message)?;
    write_all_nonblocking(stream, &encoded)
}

fn parse_attach_target(raw: Option<&str>, option_name: &str) -> io::Result<Option<AttachTarget>> {
    match raw {
        Some(value) => AttachTarget::parse(value).map(Some).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {option_name} target: {err}"),
            )
        }),
        None => Ok(None),
    }
}

fn write_all_nonblocking(stream: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut offset = 0usize;
    while offset < data.len() {
        match stream.write(&data[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "socket write returned 0 bytes",
                ));
            }
            Ok(n) => offset += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out writing to socket",
                    ));
                }
                thread::sleep(IO_RETRY_DELAY);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn client_identity_fingerprint() -> Option<String> {
    if let Ok(identity) = std::env::var("SPECTRA_CLIENT_IDENTITY") {
        let trimmed = identity.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let tty = ["/proc/self/fd/0", "/dev/fd/0"]
        .iter()
        .find_map(|path| std::fs::read_link(path).ok())
        .map(|path| path.to_string_lossy().to_string());
    let uid = std::env::var("UID").ok();
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok());

    let mut fields = Vec::new();
    if let Some(tty) = tty
        && !tty.trim().is_empty()
    {
        fields.push(format!("tty={tty}"));
    }
    if let Some(uid) = uid
        && !uid.trim().is_empty()
    {
        fields.push(format!("uid={uid}"));
    }
    if let Some(host) = host
        && !host.trim().is_empty()
    {
        fields.push(format!("host={host}"));
    }

    if fields.is_empty() {
        None
    } else {
        Some(fields.join("|"))
    }
}

fn inside_spectra_session() -> bool {
    std::env::var("SPECTRA")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{client_identity_fingerprint, nested_session_warning};
    use crate::cli::CliMode;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn client_identity_prefers_env_override() {
        let _guard = env_lock().lock().expect("env lock");
        let previous = std::env::var("SPECTRA_CLIENT_IDENTITY").ok();
        // SAFETY: The test serializes environment updates with a process-wide mutex.
        unsafe { std::env::set_var("SPECTRA_CLIENT_IDENTITY", "manual-test-id") };
        let identity = client_identity_fingerprint();
        assert_eq!(identity.as_deref(), Some("manual-test-id"));
        if let Some(previous) = previous {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::set_var("SPECTRA_CLIENT_IDENTITY", previous) };
        } else {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::remove_var("SPECTRA_CLIENT_IDENTITY") };
        }
    }

    #[test]
    fn nested_warning_shows_for_attach_mode_when_inside_spectra() {
        let _guard = env_lock().lock().expect("env lock");
        let previous = std::env::var("SPECTRA").ok();
        // SAFETY: The test serializes environment updates with a process-wide mutex.
        unsafe { std::env::set_var("SPECTRA", "1") };

        assert_eq!(
            nested_session_warning(CliMode::AttachOrCreate),
            Some("sessions should be nested with care, unset $SPECTRA to force")
        );

        if let Some(previous) = previous {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::set_var("SPECTRA", previous) };
        } else {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::remove_var("SPECTRA") };
        }
    }

    #[test]
    fn nested_warning_is_not_shown_outside_attach_mode() {
        let _guard = env_lock().lock().expect("env lock");
        let previous = std::env::var("SPECTRA").ok();
        // SAFETY: The test serializes environment updates with a process-wide mutex.
        unsafe { std::env::set_var("SPECTRA", "1") };

        assert_eq!(nested_session_warning(CliMode::RunCommand), None);
        assert_eq!(nested_session_warning(CliMode::RunServer), None);

        if let Some(previous) = previous {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::set_var("SPECTRA", previous) };
        } else {
            // SAFETY: The test serializes environment updates with a process-wide mutex.
            unsafe { std::env::remove_var("SPECTRA") };
        }
    }
}
