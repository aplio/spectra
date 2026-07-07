#![cfg(unix)]

//! Heavy-output flood throughput benchmark.
//!
//! Measures how fast the full pipeline (pane pty read -> VT parse/grid ->
//! render -> client socket) ingests a sustained burst of guest output, as a
//! counterpart to the input-latency numbers from `socket_latency_e2e`. The
//! pane shell evals pasted commands, so each scenario pastes a one-line
//! producer (`yes | head`, `awk`) followed by a unique completion marker and
//! times paste-to-marker-render. Throughput is payload bytes over that wall
//! time, so it includes producer cost; the producers are chosen to run far
//! faster than the pipeline under test.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spectra::ipc::codec::{decode_messages, encode_message};
use spectra::ipc::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);
const WAIT_TIMEOUT: Duration = Duration::from_secs(6);
const FLOOD_TIMEOUT: Duration = Duration::from_secs(120);
const WARMUP_RUNS_DEFAULT: usize = 1;
const MEASURE_RUNS_DEFAULT: usize = 3;
const FLOOD_LINES_DEFAULT: usize = 200_000;

/// 52 ASCII chars; with the trailing newline each flooded line is 53 bytes.
const ASCII_LINE_BODY: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP";

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
        };
        client.send(ClientMessage::Hello {
            cols,
            rows,
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

    /// Drains renders until one contains `marker` in its raw ANSI payload.
    ///
    /// The flood marker is unique and printed as a plain line, so substring
    /// search over the raw frame is enough — no screen model needed. The
    /// drain loop sleeps only 1ms so the test client never becomes the
    /// backpressure point of the pipeline being measured.
    fn wait_for_render_marker(
        &mut self,
        marker: &str,
        timeout: Duration,
    ) -> io::Result<RenderVolume> {
        let deadline = Instant::now() + timeout;
        let mut volume = RenderVolume::default();

        loop {
            for message in self.read_messages()? {
                if let ServerMessage::Render { ansi } = message {
                    volume.renders += 1;
                    volume.bytes += ansi.len() as u64;
                    if ansi.contains(marker) {
                        if std::env::var("SPECTRA_FLOOD_DEBUG").is_ok() {
                            eprintln!(
                                "FLOOD_DEBUG marker={marker} frame_len={} frame={:?}",
                                ansi.len(),
                                &ansi[..ansi.len().min(600)]
                            );
                        }
                        return Ok(volume);
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for flood marker {marker}; renders={} render_bytes={}",
                        volume.renders, volume.bytes
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Drains messages until none arrive for `quiet`, so late renders from
    /// one run can never bleed into the next run's timing window.
    fn drain_until_quiet(&mut self, quiet: Duration) -> io::Result<()> {
        let mut last_message = Instant::now();
        loop {
            if !self.read_messages()?.is_empty() {
                last_message = Instant::now();
            }
            if last_message.elapsed() >= quiet {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_messages(&mut self) -> io::Result<Vec<ServerMessage>> {
        let mut chunk = [0u8; 65536];
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

#[derive(Debug, Default, Clone, Copy)]
struct RenderVolume {
    renders: u64,
    bytes: u64,
}

fn spawn_server(runtime_dir: &Path, data_home: &Path) -> io::Result<ServerProcess> {
    let bin = resolve_spectra_binary()?;
    let config_home = data_home.join("config-home");
    std::fs::create_dir_all(&config_home)?;

    // The pane shell evals each pasted line, so scenarios can run arbitrary
    // producers inside the pty.
    let child = Command::new(bin)
        .arg("--server")
        .arg("--shell")
        .arg("/bin/sh")
        .arg("--")
        .arg("stty -echo; while IFS= read -r line; do eval \"$line\"; done")
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
        "could not locate spectra binary for flood throughput e2e test",
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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Markers embed the scenario name and run index verbatim: hashing
/// scenario+index into one integer collided across scenarios (ascii run N
/// and sgr run N-2 hashed identically), so a leftover marker from the
/// previous scenario still on screen matched instantly.
fn marker_for(scenario: &str, index: usize) -> String {
    format!("FLOOD_DONE_{scenario}_{index}_end")
}

struct Scenario {
    name: &'static str,
    /// One-line shell producer; must print exactly `payload_bytes` and then
    /// return, after which the marker line is printed.
    command: String,
    payload_bytes: u64,
}

fn ascii_scenario(lines: usize) -> Scenario {
    Scenario {
        name: "ascii_lines",
        command: format!("yes '{ASCII_LINE_BODY}' | head -n {lines}"),
        payload_bytes: (ASCII_LINE_BODY.len() as u64 + 1) * lines as u64,
    }
}

fn sgr_scenario(lines: usize) -> Scenario {
    // Each line is ESC[3Nm (5 bytes, N in 0..8) + 46 chars + ESC[0m (4 bytes)
    // + newline = 56 bytes.
    let body = "the-quick-brown-fox-jumps-over-the-lazy-dog-01";
    assert_eq!(body.len(), 46);
    Scenario {
        name: "sgr_lines",
        command: format!(
            "awk 'BEGIN{{for(i=0;i<{lines};i++)printf \"\\033[3%dm{body}\\033[0m\\n\", i%8}}'"
        ),
        payload_bytes: 56 * lines as u64,
    }
}

#[derive(Debug)]
struct FloodRun {
    elapsed_s: f64,
    mb_per_s: f64,
    volume: RenderVolume,
}

fn run_flood(client: &mut TestClient, scenario: &Scenario, marker: &str) -> io::Result<FloodRun> {
    let started = Instant::now();
    client.send(ClientMessage::Paste {
        text: format!("{}; printf '%s\\n' '{marker}'\n", scenario.command),
    })?;
    let volume = client.wait_for_render_marker(marker, FLOOD_TIMEOUT)?;
    let elapsed_s = started.elapsed().as_secs_f64();
    let mb_per_s = scenario.payload_bytes as f64 / 1_000_000.0 / elapsed_s;
    Ok(FloodRun {
        elapsed_s,
        mb_per_s,
        volume,
    })
}

fn median(sorted_values: &mut [f64]) -> f64 {
    sorted_values.sort_by(f64::total_cmp);
    let n = sorted_values.len();
    assert!(n > 0, "flood run set must not be empty");
    if n % 2 == 1 {
        sorted_values[n / 2]
    } else {
        (sorted_values[n / 2 - 1] + sorted_values[n / 2]) / 2.0
    }
}

fn measure_scenario(
    client: &mut TestClient,
    scenario: &Scenario,
    warmup: usize,
    measured: usize,
) -> io::Result<Vec<FloodRun>> {
    let mut runs = Vec::with_capacity(measured);
    for index in 0..(warmup + measured) {
        let marker = marker_for(scenario.name, index);
        let run = run_flood(client, scenario, &marker)?;
        client.drain_until_quiet(Duration::from_millis(300))?;
        if index >= warmup {
            runs.push(run);
        }
    }
    Ok(runs)
}

fn report_scenario(scenario: &Scenario, warmup: usize, runs: &[FloodRun]) {
    let mut rates: Vec<f64> = runs.iter().map(|run| run.mb_per_s).collect();
    let median_rate = median(&mut rates);
    let mut elapsed: Vec<f64> = runs.iter().map(|run| run.elapsed_s).collect();
    let median_elapsed = median(&mut elapsed);
    let renders: u64 = runs.iter().map(|run| run.volume.renders).sum();
    let render_bytes: u64 = runs.iter().map(|run| run.volume.bytes).sum();

    println!(
        "FLOOD_RESULT scenario={} runs={} warmup={} payload_mb={:.2} median_s={:.3} median_mb_s={:.2} renders={} render_bytes={}",
        scenario.name,
        runs.len(),
        warmup,
        scenario.payload_bytes as f64 / 1_000_000.0,
        median_elapsed,
        median_rate,
        renders,
        render_bytes
    );
    for (index, run) in runs.iter().enumerate() {
        println!(
            "FLOOD_RUN scenario={} run={} elapsed_s={:.3} mb_s={:.2} renders={} render_bytes={}",
            scenario.name, index, run.elapsed_s, run.mb_per_s, run.volume.renders, run.volume.bytes
        );
    }
}

#[test]
fn flood_throughput_reports_sustained_output_rate() {
    let warmup = env_usize("SPECTRA_FLOOD_WARMUP", WARMUP_RUNS_DEFAULT);
    let measured = env_usize("SPECTRA_FLOOD_RUNS", MEASURE_RUNS_DEFAULT);
    let lines = env_usize("SPECTRA_FLOOD_LINES", FLOOD_LINES_DEFAULT);

    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let data_home = dir.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&data_home).expect("create data dir");

    let server = spawn_server(&runtime_dir, &data_home).expect("spawn server");
    let socket = socket_path(&runtime_dir);
    wait_for_socket(&socket).expect("wait for socket");

    let mut client = TestClient::connect(&socket, 120, 32).expect("connect client");
    client
        .wait_for_message(WAIT_TIMEOUT, |message| {
            matches!(message, ServerMessage::Render { .. })
        })
        .expect("initial render");

    let scenarios = [ascii_scenario(lines), sgr_scenario(lines)];
    for scenario in &scenarios {
        let runs =
            measure_scenario(&mut client, scenario, warmup, measured).expect("measure flood");
        assert_eq!(runs.len(), measured);
        report_scenario(scenario, warmup, &runs);
    }

    drop(client);
    drop(server);
}
