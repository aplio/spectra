//! Plugin service supervision: one long-running child per `[service]`
//! plugin, restarted with capped exponential backoff, killed on drop.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::Logger;

/// File in the plugin directory receiving the service's stdout/stderr.
pub const SERVICE_LOG_FILE: &str = "service.log";
/// The service log is truncated at supervisor start when larger than this.
const SERVICE_LOG_TRUNCATE_BYTES: u64 = 1024 * 1024;

/// Supervision timing knobs; tests inject fast values.
#[derive(Debug, Clone, Copy)]
pub struct ServiceTuning {
    /// Delay before the first restart; doubles per crash up to `max_backoff`.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Sliding window for crash-loop detection. A run at least this long
    /// also resets the backoff and the crash count.
    pub crash_window: Duration,
    /// Restarting stops once this many exits happened within `crash_window`.
    pub max_crashes_in_window: usize,
    /// How often the monitor thread polls the child and the stop flag.
    pub poll_interval: Duration,
}

impl Default for ServiceTuning {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            crash_window: Duration::from_secs(60),
            max_crashes_in_window: 5,
            poll_interval: Duration::from_millis(25),
        }
    }
}

/// Handle for one supervised plugin service. Dropping it stops the monitor
/// thread and kills the current child (best effort).
pub(crate) struct ServiceSupervisor {
    command: Vec<String>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    handle: Option<JoinHandle<()>>,
}

impl ServiceSupervisor {
    /// Spawn the monitor thread for one plugin service. The service child is
    /// spawned (and respawned) by the monitor, never by the caller.
    pub fn start(
        plugin: &str,
        dir: &Path,
        argv: &[String],
        envs: Vec<(String, String)>,
        logger: Logger,
        tuning: ServiceTuning,
    ) -> Result<Self, String> {
        truncate_oversized_log(&dir.join(SERVICE_LOG_FILE));

        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let monitor = Monitor {
            plugin: plugin.to_string(),
            dir: dir.to_path_buf(),
            argv: argv.to_vec(),
            envs,
            logger,
            tuning,
            stop: Arc::clone(&stop),
            child: Arc::clone(&child),
        };
        let handle = thread::Builder::new()
            .name(format!("spectra-plugin-{plugin}"))
            .spawn(move || monitor.run())
            .map_err(|err| format!("service monitor thread spawn failed: {err}"))?;

        Ok(Self {
            command: argv.to_vec(),
            stop,
            child,
            handle: Some(handle),
        })
    }

    /// Argv this supervisor was started with (rescan change detection).
    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// PID of the currently running child, if any (used by tests).
    #[cfg(test)]
    pub fn child_pid(&self) -> Option<u32> {
        lock_child(&self.child).as_ref().map(Child::id)
    }
}

impl Drop for ServiceSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        {
            let mut guard = lock_child(&self.child);
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Monitor {
    plugin: String,
    dir: PathBuf,
    argv: Vec<String>,
    envs: Vec<(String, String)>,
    logger: Logger,
    tuning: ServiceTuning,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Monitor {
    fn run(self) {
        let mut backoff = self.tuning.initial_backoff;
        let mut crashes: Vec<Instant> = Vec::new();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }

            let started_at = Instant::now();
            match self.spawn_child() {
                Err(err) => {
                    (self.logger)(&format!(
                        "plugin {}: service spawn failed: {err}",
                        self.plugin
                    ));
                }
                Ok(child) => {
                    *lock_child(&self.child) = Some(child);
                    let Some(status) = self.wait_for_exit() else {
                        // Stop requested; the child was killed and reaped.
                        return;
                    };
                    if started_at.elapsed() >= self.tuning.crash_window {
                        // A long healthy run resets the crash bookkeeping.
                        crashes.clear();
                        backoff = self.tuning.initial_backoff;
                    }
                    (self.logger)(&format!(
                        "plugin {}: service exited ({status}); restarting in {backoff:?}",
                        self.plugin
                    ));
                }
            }

            let now = Instant::now();
            crashes.push(now);
            crashes.retain(|at| now.duration_since(*at) <= self.tuning.crash_window);
            if crashes.len() >= self.tuning.max_crashes_in_window {
                (self.logger)(&format!(
                    "plugin {}: service exited {} times within {:?}; not restarting",
                    self.plugin,
                    crashes.len(),
                    self.tuning.crash_window
                ));
                return;
            }

            if !self.sleep_unless_stopped(backoff) {
                return;
            }
            backoff = (backoff * 2).min(self.tuning.max_backoff);
        }
    }

    fn spawn_child(&self) -> Result<Child, String> {
        let Some(program) = self.argv.first() else {
            return Err("empty service argv".to_string());
        };
        let mut command = Command::new(program);
        command
            .args(&self.argv[1..])
            .current_dir(&self.dir)
            .envs(self.envs.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null());

        // Append stdout/stderr to the plugin's service.log; fall back to
        // discarding output when the log file cannot be opened.
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(SERVICE_LOG_FILE));
        match log.and_then(|file| Ok((file.try_clone()?, file))) {
            Ok((stdout, stderr)) => {
                command.stdout(stdout).stderr(stderr);
            }
            Err(_) => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }

        command.spawn().map_err(|err| err.to_string())
    }

    /// Poll the child until it exits (returning a status description) or the
    /// stop flag is raised (killing and reaping the child, returning None).
    fn wait_for_exit(&self) -> Option<String> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                let mut guard = lock_child(&self.child);
                if let Some(child) = guard.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                *guard = None;
                return None;
            }
            {
                let mut guard = lock_child(&self.child);
                let child = guard.as_mut()?;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        return Some(status.to_string());
                    }
                    Ok(None) => {}
                    Err(err) => {
                        *guard = None;
                        return Some(format!("wait failed: {err}"));
                    }
                }
            }
            thread::sleep(self.tuning.poll_interval);
        }
    }

    /// Sleep `total` in poll-interval slices; false when stop was requested.
    fn sleep_unless_stopped(&self, total: Duration) -> bool {
        let deadline = Instant::now() + total;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return true;
            };
            if remaining.is_zero() {
                return true;
            }
            thread::sleep(remaining.min(self.tuning.poll_interval));
        }
    }
}

/// Lock the shared child slot, recovering from a poisoned mutex (a panicked
/// monitor thread must never take the server down with it).
fn lock_child(child: &Arc<Mutex<Option<Child>>>) -> MutexGuard<'_, Option<Child>> {
    child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Truncate the service log when it grew beyond the size cap.
fn truncate_oversized_log(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() > SERVICE_LOG_TRUNCATE_BYTES {
        let _ = std::fs::write(path, b"");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_logger() -> Logger {
        Arc::new(|_line: &str| {})
    }

    fn collecting_logger() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let logger: Logger = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_string());
        });
        (logger, lines)
    }

    fn fast_tuning() -> ServiceTuning {
        ServiceTuning {
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            crash_window: Duration::from_secs(60),
            max_crashes_in_window: 100,
            poll_interval: Duration::from_millis(5),
        }
    }

    fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
    }

    #[test]
    fn service_spawns_and_captures_output_in_service_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = ServiceSupervisor::start(
            "logger",
            dir.path(),
            &sh("echo hello-from-service; echo oops >&2; sleep 30"),
            Vec::new(),
            noop_logger(),
            fast_tuning(),
        )
        .expect("start supervisor");

        let log_path = dir.path().join(SERVICE_LOG_FILE);
        assert!(wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&log_path)
                .is_ok_and(|log| log.contains("hello-from-service") && log.contains("oops"))
        }));
        drop(supervisor);
    }

    #[test]
    fn exiting_service_is_restarted_with_backoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = dir.path().join("runs.txt");
        let supervisor = ServiceSupervisor::start(
            "flappy",
            dir.path(),
            &sh("echo run >> runs.txt; exit 1"),
            Vec::new(),
            noop_logger(),
            fast_tuning(),
        )
        .expect("start supervisor");

        assert!(wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&runs).is_ok_and(|content| content.lines().count() >= 3)
        }));
        drop(supervisor);
    }

    #[test]
    fn crash_loop_stops_restarting_after_cutoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = dir.path().join("runs.txt");
        let (logger, lines) = collecting_logger();
        let tuning = ServiceTuning {
            max_crashes_in_window: 3,
            ..fast_tuning()
        };
        let supervisor = ServiceSupervisor::start(
            "crashy",
            dir.path(),
            &sh("echo run >> runs.txt; exit 7"),
            Vec::new(),
            logger,
            tuning,
        )
        .expect("start supervisor");

        assert!(wait_until(Duration::from_secs(5), || {
            lines
                .lock()
                .expect("log lock")
                .iter()
                .any(|line| line.contains("not restarting"))
        }));
        // No further runs happen after the cutoff.
        let count = std::fs::read_to_string(&runs)
            .expect("runs file")
            .lines()
            .count();
        assert_eq!(count, 3);
        thread::sleep(Duration::from_millis(100));
        let after = std::fs::read_to_string(&runs)
            .expect("runs file")
            .lines()
            .count();
        assert_eq!(after, count);
        drop(supervisor);
    }

    #[test]
    fn dropping_supervisor_kills_running_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = ServiceSupervisor::start(
            "sleeper",
            dir.path(),
            &sh("sleep 30"),
            Vec::new(),
            noop_logger(),
            fast_tuning(),
        )
        .expect("start supervisor");

        assert!(wait_until(Duration::from_secs(5), || {
            supervisor.child_pid().is_some()
        }));
        let pid = supervisor.child_pid().expect("running child pid");
        drop(supervisor);

        // Drop kills and reaps the child, so its /proc entry disappears.
        assert!(wait_until(Duration::from_secs(5), || {
            !std::path::Path::new(&format!("/proc/{pid}")).exists()
        }));
    }

    #[test]
    fn oversized_service_log_is_truncated_at_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(SERVICE_LOG_FILE);
        std::fs::write(&log_path, vec![b'x'; 2 * 1024 * 1024]).expect("write oversized log");

        let supervisor = ServiceSupervisor::start(
            "trunc",
            dir.path(),
            &sh("sleep 30"),
            Vec::new(),
            noop_logger(),
            fast_tuning(),
        )
        .expect("start supervisor");

        // Truncation happens synchronously in start().
        let len = std::fs::metadata(&log_path).expect("log metadata").len();
        assert!(len < 1024 * 1024, "log not truncated: {len} bytes");
        drop(supervisor);
    }

    #[test]
    fn service_env_and_cwd_are_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = ServiceSupervisor::start(
            "envy",
            dir.path(),
            &sh("printf '%s' \"$SPECTRA_API_SOCKET\" > env.txt; sleep 30"),
            vec![(
                "SPECTRA_API_SOCKET".to_string(),
                "/tmp/test-api.sock".to_string(),
            )],
            noop_logger(),
            fast_tuning(),
        )
        .expect("start supervisor");

        let out = dir.path().join("env.txt");
        assert!(wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&out).is_ok_and(|content| content == "/tmp/test-api.sock")
        }));
        drop(supervisor);
    }
}
