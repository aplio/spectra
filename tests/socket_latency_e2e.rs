#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{ClientMessage, NetKeyEvent, ServerMessage};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(6);
const WARMUP_SAMPLES_DEFAULT: usize = 10;
const MEASURE_SAMPLES_DEFAULT: usize = 80;

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
            attach_target: None,
            client_identity: None,
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

    fn send_key_char(&mut self, ch: char) -> io::Result<()> {
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
    }

    fn send_text_as_keys(&mut self, text: &str) -> io::Result<()> {
        for ch in text.chars() {
            self.send_key_char(ch)?;
        }
        Ok(())
    }

    fn send_enter(&mut self) -> io::Result<()> {
        self.send_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
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
        let mut render_count = 0usize;
        let mut non_render_count = 0usize;
        let mut last_render_len = 0usize;

        loop {
            for message in self.read_messages()? {
                match message {
                    ServerMessage::Render { ansi } => {
                        render_count += 1;
                        last_render_len = ansi.len();
                        self.screen.apply_ansi(&ansi);
                        if self.screen.contains(needle) {
                            return Ok(());
                        }
                    }
                    _ => {
                        non_render_count += 1;
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for render containing marker: {needle}; render_count={render_count}; non_render_count={non_render_count}; last_render_len={last_render_len}"
                    ),
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

fn spawn_server(runtime_dir: &Path, data_home: &Path) -> io::Result<ServerProcess> {
    let bin = resolve_spectra_binary()?;
    let config_home = data_home.join("config-home");
    std::fs::create_dir_all(&config_home)?;

    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("stty -echo; while IFS= read -r line; do printf '%s\\n' \"$line\"; done")
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
        "could not locate spectra binary for socket latency e2e test",
    ))
}

fn wait_for_socket(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for socket: {}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("spectra").join("spectra.sock")
}

#[derive(Debug)]
struct LatencyStats {
    samples: usize,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

fn compute_stats(samples_ms: &[f64]) -> LatencyStats {
    assert!(
        !samples_ms.is_empty(),
        "latency sample set must not be empty"
    );

    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);

    let sum: f64 = sorted.iter().sum();
    let mean_ms = sum / (sorted.len() as f64);
    let p50_ms = percentile(&sorted, 0.50);
    let p95_ms = percentile(&sorted, 0.95);
    let max_ms = sorted.last().copied().unwrap_or_default();

    LatencyStats {
        samples: sorted.len(),
        mean_ms,
        p50_ms,
        p95_ms,
        max_ms,
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let clamped = quantile.clamp(0.0, 1.0);
    let rank = ((sorted.len() - 1) as f64 * clamped).round() as usize;
    sorted[rank]
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn marker_for(seed: u64, index: usize) -> String {
    let mut value = (index as u64).wrapping_add(seed);
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^= value >> 33;
    format!("{value:016x}")
}

fn measure_paste_latency(
    client: &mut TestClient,
    warmup: usize,
    measured: usize,
) -> io::Result<LatencyStats> {
    let mut samples_ms = Vec::with_capacity(measured);
    let seed = 0x12A4_55EE_91C3_0F0Du64;

    for index in 0..(warmup + measured) {
        let marker = marker_for(seed, index);
        let started = Instant::now();
        client.send(ClientMessage::Paste {
            text: format!("{marker}\n"),
        })?;
        client.wait_for_render_containing(&marker, WAIT_TIMEOUT)?;
        if index >= warmup {
            samples_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }

    Ok(compute_stats(&samples_ms))
}

fn measure_key_latency(
    client: &mut TestClient,
    warmup: usize,
    measured: usize,
) -> io::Result<LatencyStats> {
    let mut samples_ms = Vec::with_capacity(measured);
    let seed = 0xE7D1_2039_5B4A_776Cu64;

    for index in 0..(warmup + measured) {
        let marker = marker_for(seed, index);
        let started = Instant::now();
        client.send_text_as_keys(&marker)?;
        client.send_enter()?;
        client.wait_for_render_containing(&marker, WAIT_TIMEOUT)?;
        if index >= warmup {
            samples_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }

    Ok(compute_stats(&samples_ms))
}

#[test]
fn socket_latency_batch_reports_paste_and_key_roundtrip() {
    let warmup = env_usize("SPECTRA_LATENCY_WARMUP", WARMUP_SAMPLES_DEFAULT);
    let measured = env_usize("SPECTRA_LATENCY_SAMPLES", MEASURE_SAMPLES_DEFAULT);

    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let mut server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = TestClient::connect(&socket, 120, 32).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    let paste_stats =
        measure_paste_latency(&mut client, warmup, measured).expect("measure paste latency");
    let key_stats =
        measure_key_latency(&mut client, warmup, measured).expect("measure key latency");

    println!(
        "LATENCY_RESULT scenario=paste samples={} warmup={} measured={} mean_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        paste_stats.samples,
        warmup,
        measured,
        paste_stats.mean_ms,
        paste_stats.p50_ms,
        paste_stats.p95_ms,
        paste_stats.max_ms
    );
    println!(
        "LATENCY_RESULT scenario=key samples={} warmup={} measured={} mean_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        key_stats.samples,
        warmup,
        measured,
        key_stats.mean_ms,
        key_stats.p50_ms,
        key_stats.p95_ms,
        key_stats.max_ms
    );

    assert_eq!(paste_stats.samples, measured);
    assert_eq!(key_stats.samples, measured);

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
