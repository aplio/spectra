#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use spectra::attach_target::AttachTarget;
use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{
    ClientMessage, CommandRequest, CommandResult, CommandSplitAxis, NetKeyEvent, ServerMessage,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(4);

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
    screen: ScreenState,
}

struct ScreenState {
    cells: Vec<Vec<char>>,
    cursor_x: usize,
    cursor_y: usize,
    cols: usize,
    rows: usize,
}

impl ScreenState {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cells: vec![vec![' '; cols]; rows],
            cursor_x: 0,
            cursor_y: 0,
            cols,
            rows,
        }
    }

    fn clear_all(&mut self) {
        for row in &mut self.cells {
            for cell in row {
                *cell = ' ';
            }
        }
    }

    fn contains(&self, needle: &str) -> bool {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>())
            .any(|row| row.contains(needle))
    }

    fn apply_ansi(&mut self, ansi: &str) {
        let bytes = ansi.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            match bytes[i] {
                b'\x1b' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                        if let Some((next, final_byte, params)) = parse_csi(bytes, i + 2) {
                            self.apply_csi(final_byte, params);
                            i = next;
                        } else {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                b'\r' => {
                    self.cursor_x = 0;
                    i += 1;
                }
                b'\n' => {
                    self.cursor_y = (self.cursor_y + 1).min(self.rows.saturating_sub(1));
                    i += 1;
                }
                _ => {
                    let s = std::str::from_utf8(&bytes[i..]).expect("valid utf-8 render payload");
                    let ch = s.chars().next().expect("char exists");
                    if self.cursor_y < self.rows && self.cursor_x < self.cols {
                        self.cells[self.cursor_y][self.cursor_x] = ch;
                    }
                    self.cursor_x = (self.cursor_x + 1).min(self.cols.saturating_sub(1));
                    i += ch.len_utf8();
                }
            }
        }
    }

    fn apply_csi(&mut self, final_byte: u8, params: &str) {
        match final_byte {
            b'H' => {
                let (row, col) = parse_cursor_position(params);
                self.cursor_y = row.min(self.rows.saturating_sub(1));
                self.cursor_x = col.min(self.cols.saturating_sub(1));
            }
            b'J' if params == "2" => {
                self.clear_all();
            }
            b'K' => {
                let mode = if params.is_empty() { "0" } else { params };
                if self.rows == 0 || self.cols == 0 || self.cursor_y >= self.rows {
                    return;
                }
                match mode {
                    "1" => {
                        for col in 0..=self.cursor_x.min(self.cols.saturating_sub(1)) {
                            self.cells[self.cursor_y][col] = ' ';
                        }
                    }
                    "2" => {
                        for col in 0..self.cols {
                            self.cells[self.cursor_y][col] = ' ';
                        }
                    }
                    _ => {
                        for col in self.cursor_x.min(self.cols)..self.cols {
                            self.cells[self.cursor_y][col] = ' ';
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl TestClient {
    fn connect(socket: &Path, cols: u16, rows: u16) -> io::Result<Self> {
        Self::connect_with_target(socket, cols, rows, None)
    }

    fn connect_with_target(
        socket: &Path,
        cols: u16,
        rows: u16,
        attach_target: Option<AttachTarget>,
    ) -> io::Result<Self> {
        Self::connect_with_target_and_identity(socket, cols, rows, attach_target, None)
    }

    fn connect_with_target_and_identity(
        socket: &Path,
        cols: u16,
        rows: u16,
        attach_target: Option<AttachTarget>,
        client_identity: Option<String>,
    ) -> io::Result<Self> {
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
            screen: ScreenState::new(cols as usize, rows as usize),
        };
        client.send(ClientMessage::Hello {
            cols,
            rows,
            attach_target,
            client_identity,
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

    fn send_detach(&mut self) -> io::Result<()> {
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ))?;
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Char('d'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
    }

    fn send_quit(&mut self) -> io::Result<()> {
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ))?;
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
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
        let deadline = Instant::now() + timeout;
        loop {
            for message in self.read_messages()? {
                if let ServerMessage::Render { ansi } = message {
                    if ansi.contains(needle) {
                        return Ok(());
                    }
                    self.screen.apply_ansi(&ansi);
                    if self.screen.contains(needle) {
                        return Ok(());
                    }
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
    let candidate_exe = target_dir.join("spectra.exe");
    if candidate_exe.exists() {
        return Ok(candidate_exe);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not locate spectra binary for socket e2e test",
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

fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("spectra").join("spectra.sock")
}

fn parse_csi(bytes: &[u8], start: usize) -> Option<(usize, u8, &str)> {
    let mut idx = start;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if (0x40..=0x7e).contains(&byte) {
            let params = std::str::from_utf8(&bytes[start..idx]).ok()?;
            return Some((idx + 1, byte, params));
        }
        idx += 1;
    }
    None
}

fn parse_cursor_position(params: &str) -> (usize, usize) {
    let mut parts = params.split(';');
    let row_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let col_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    (row_1_based.saturating_sub(1), col_1_based.saturating_sub(1))
}

#[test]
fn socket_attach_detach_and_reattach_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client_a = TestClient::connect(&socket, 80, 24).expect("connect client A");
    let mut client_b = TestClient::connect(&socket, 90, 30).expect("connect client B");

    client_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for client A");
    client_b
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for client B");

    client_a
        .send(ClientMessage::Paste {
            text: "alpha\r".to_string(),
        })
        .expect("client A write");
    client_b
        .send(ClientMessage::Paste {
            text: "beta\r".to_string(),
        })
        .expect("client B write");
    client_a
        .wait_for_render_containing("alpha", WAIT_TIMEOUT)
        .expect("alpha visible");
    client_b
        .wait_for_render_containing("beta", WAIT_TIMEOUT)
        .expect("beta visible");

    client_a.send_detach().expect("detach client A");
    client_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Detached { .. })
        })
        .expect("detached event for A");

    client_b
        .send(ClientMessage::Paste {
            text: "still-here\r".to_string(),
        })
        .expect("write after A detach");
    client_b
        .wait_for_render_containing("still-here", WAIT_TIMEOUT)
        .expect("B still attached");

    client_b.send_detach().expect("detach client B");
    client_b
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Detached { .. })
        })
        .expect("detached event for B");

    let mut client_c = TestClient::connect(&socket, 100, 32).expect("connect client C");
    client_c
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for C");
    client_c
        .send(ClientMessage::Paste {
            text: "reattach\r".to_string(),
        })
        .expect("write after reattach");
    client_c
        .wait_for_render_containing("reattach", WAIT_TIMEOUT)
        .expect("reattach output");

    let mut client_d = TestClient::connect(&socket, 120, 20).expect("connect client D");
    client_d
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for D");

    client_c
        .send(ClientMessage::Resize {
            cols: 110,
            rows: 40,
        })
        .expect("resize from C");
    client_c
        .wait_for_render_containing("\x1b[40;1H", WAIT_TIMEOUT)
        .expect("client C uses its resized viewport");
    client_d
        .send(ClientMessage::Paste {
            text: "delta\r".to_string(),
        })
        .expect("write after C resize");
    let message = client_d
        .wait_for_message(WAIT_TIMEOUT, |message| match message {
            ServerMessage::Render { ansi } => ansi.contains("delta"),
            _ => false,
        })
        .expect("client D render after own input");
    let ServerMessage::Render { ansi } = message else {
        panic!("expected render for D");
    };
    assert!(
        !ansi.contains("\x1b[40;1H"),
        "client D should not adopt client C viewport"
    );

    client_c.send_quit().expect("quit from C");
    let _ = client_c
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
fn socket_long_multiline_output_auto_scrolls_and_accepts_immediate_followup_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = TestClient::connect(&socket, 80, 8).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    let long_timeout = Duration::from_secs(12);
    for index in 0..180 {
        client
            .send(ClientMessage::Paste {
                text: format!("bulk-{index:03}\r"),
            })
            .expect("send bulk line");
    }
    client
        .send(ClientMessage::Paste {
            text: "bulk-END\r".to_string(),
        })
        .expect("send sentinel line");
    client
        .wait_for_render_containing("bulk-END", long_timeout)
        .expect("tail marker should become visible");

    client
        .send(ClientMessage::Paste {
            text: "after-burst\r".to_string(),
        })
        .expect("send immediate follow-up input");
    client
        .wait_for_render_containing("after-burst", long_timeout)
        .expect("follow-up input should render immediately");

    client.send_quit().expect("quit from client");
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
fn socket_attach_with_valid_target_starts_attached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let target = AttachTarget::parse("s1:w1.p1").expect("parse attach target");
    let mut client = TestClient::connect_with_target(&socket, 80, 24, Some(target))
        .expect("connect with attach target");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("render for attach client");

    client
        .send(ClientMessage::Paste {
            text: "target-ok\r".to_string(),
        })
        .expect("write after attach");
    client
        .wait_for_render_containing("target-ok", WAIT_TIMEOUT)
        .expect("target output");

    client.send_quit().expect("quit from target client");
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
fn socket_clients_keep_distinct_active_sessions_after_attach() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client_a = TestClient::connect(&socket, 90, 28).expect("connect client A");
    client_a
        .wait_for_render_containing("session 1/1", WAIT_TIMEOUT)
        .expect("initial session on A");

    let mut command_client = TestClient::connect(&socket, 80, 24).expect("connect command client");
    command_client
        .send(ClientMessage::Command {
            request: CommandRequest::NewSession,
        })
        .expect("send new-session command");
    let message = command_client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::CommandResult { .. })
        })
        .expect("new-session command result");
    match message {
        ServerMessage::CommandResult { result } => match result {
            CommandResult::Message { message } => {
                assert!(
                    message.contains("session created"),
                    "unexpected command result: {message}"
                );
            }
            other => panic!("unexpected command result: {other:?}"),
        },
        other => panic!("unexpected server message: {other:?}"),
    }

    client_a
        .wait_for_render_containing("session 1/2", WAIT_TIMEOUT)
        .expect("A stays on first session after second session is created");

    let target = AttachTarget::parse("s2").expect("parse attach target");
    let mut client_b = TestClient::connect_with_target(&socket, 110, 22, Some(target))
        .expect("connect client B to second session");
    client_b
        .wait_for_render_containing("session 2/2", WAIT_TIMEOUT)
        .expect("B starts on second session");
    client_a
        .wait_for_render_containing("session 1/2", WAIT_TIMEOUT)
        .expect("A remains on first session after B attaches");

    client_b.send_quit().expect("quit from B");
    let _ = client_b
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
fn socket_clients_isolate_prefix_mode_and_lock_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client_a = TestClient::connect(&socket, 90, 28).expect("connect client A");
    let mut client_b = TestClient::connect(&socket, 110, 22).expect("connect client B");

    client_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for A");
    client_b
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for B");

    client_a
        .send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("A prefix");
    client_a
        .send_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("A open cursor mode");

    client_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("A receives render after cursor command");
    let message = client_b
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("B gets render while A enters cursor mode");
    let ServerMessage::Render { ansi } = message else {
        panic!("expected render for B");
    };
    assert!(!ansi.contains("cursor mode"));
    assert!(!ansi.contains("LOCK"));

    client_a
        .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("A exit cursor mode");
    client_a
        .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("A clear sticky prefix");
    // Enter lock mode via command palette
    client_a
        .send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("A prefix for palette");
    client_a
        .send_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("A open command palette");
    for ch in "enter lock".chars() {
        client_a
            .send_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("A type lock query");
    }
    client_a
        .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("A execute enter lock mode");
    client_a
        .wait_for_render_containing("LOCK", WAIT_TIMEOUT)
        .expect("A lock indicator");

    let message = client_b
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("B gets render while A locks");
    let ServerMessage::Render { ansi } = message else {
        panic!("expected render for B");
    };
    assert!(!ansi.contains("LOCK"));
    assert!(!ansi.contains("cursor mode"));

    client_b
        .send(ClientMessage::Paste {
            text: "still-normal\r".to_string(),
        })
        .expect("B paste while A locked/cursor");
    client_b
        .wait_for_render_containing("still-normal", WAIT_TIMEOUT)
        .expect("B remains in normal mode");

    client_b.send_quit().expect("quit from B");
    let _ = client_b
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
fn socket_client_focus_profiles_restore_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut command_client = TestClient::connect(&socket, 80, 24).expect("connect command client");
    command_client
        .send(ClientMessage::Command {
            request: CommandRequest::SplitWindow {
                target: None,
                axis: CommandSplitAxis::Vertical,
            },
        })
        .expect("split window command");
    let split_result = command_client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::CommandResult { .. })
        })
        .expect("split command result");
    match split_result {
        ServerMessage::CommandResult { result } => match result {
            CommandResult::Message { message } => {
                assert!(message.contains("window split"), "unexpected: {message}");
            }
            other => panic!("unexpected split result: {other:?}"),
        },
        other => panic!("unexpected split response: {other:?}"),
    }

    let target_a = AttachTarget::parse("s1:w1.p1").expect("parse pane 1 target");
    let target_b = AttachTarget::parse("s1:w1.p2").expect("parse pane 2 target");

    let mut client_a = TestClient::connect_with_target_and_identity(
        &socket,
        90,
        28,
        Some(target_a),
        Some("persist-client-a".to_string()),
    )
    .expect("connect client A");
    let mut client_b = TestClient::connect_with_target_and_identity(
        &socket,
        110,
        22,
        Some(target_b),
        Some("persist-client-b".to_string()),
    )
    .expect("connect client B");

    client_a
        .wait_for_render_containing("pane 1/2", WAIT_TIMEOUT)
        .expect("A starts focused on pane 1");
    client_b
        .wait_for_render_containing("pane 2/2", WAIT_TIMEOUT)
        .expect("B starts focused on pane 2");

    client_a.send_quit().expect("quit first server");
    let _ = client_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Shutdown { .. })
        })
        .expect("shutdown first server");

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(status) = server.child.try_wait().expect("check first server exit") {
            assert!(
                status.success(),
                "first server exited unsuccessfully: {status}"
            );
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for first server process to exit");
        }
        thread::sleep(Duration::from_millis(20));
    }

    let mut restarted = spawn_server(&runtime_dir, &data_home).expect("restart server");
    wait_for_socket(&socket).expect("wait for restarted socket");

    let mut restored_a = TestClient::connect_with_target_and_identity(
        &socket,
        90,
        28,
        None,
        Some("persist-client-a".to_string()),
    )
    .expect("connect restored client A");
    let mut restored_b = TestClient::connect_with_target_and_identity(
        &socket,
        110,
        22,
        None,
        Some("persist-client-b".to_string()),
    )
    .expect("connect restored client B");

    restored_a
        .wait_for_render_containing("pane 1/2", WAIT_TIMEOUT)
        .expect("restored A keeps pane 1");
    restored_b
        .wait_for_render_containing("pane 2/2", WAIT_TIMEOUT)
        .expect("restored B keeps pane 2");

    restored_a.send_quit().expect("quit restarted server");
    let _ = restored_a
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Shutdown { .. })
        })
        .expect("shutdown restarted server");

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(status) = restarted
            .child
            .try_wait()
            .expect("check restarted server exit")
        {
            assert!(
                status.success(),
                "restarted server exited unsuccessfully: {status}"
            );
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for restarted server process to exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn socket_attach_with_missing_target_returns_error_and_keeps_other_clients_attached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut healthy = TestClient::connect(&socket, 80, 24).expect("connect healthy client");
    healthy
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render for healthy client");

    let missing = AttachTarget::parse("missing-session").expect("parse missing target");
    let mut failing = TestClient::connect_with_target(&socket, 80, 24, Some(missing))
        .expect("connect failing client");
    let message = failing
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Error { .. })
        })
        .expect("attach error");
    match message {
        ServerMessage::Error { message } => {
            assert!(
                message.contains("attach failed"),
                "unexpected error message: {message}"
            );
            assert!(
                message.contains("missing-session"),
                "unexpected error message: {message}"
            );
        }
        _ => panic!("expected server error"),
    }
    let disconnect = failing
        .wait_for_message(WAIT_TIMEOUT, |_| false)
        .expect_err("failing client should disconnect");
    assert_eq!(
        disconnect.kind(),
        io::ErrorKind::BrokenPipe,
        "unexpected post-error state: {disconnect}"
    );

    healthy
        .send(ClientMessage::Paste {
            text: "healthy-still-connected\r".to_string(),
        })
        .expect("healthy client write");
    healthy
        .wait_for_render_containing("healthy-still-connected", WAIT_TIMEOUT)
        .expect("healthy client still receives render");

    healthy.send_quit().expect("quit from healthy client");
    let _ = healthy
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
