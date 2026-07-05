//! Plugin system (herdr-inspired): a plugin is a directory containing a
//! `spectra-plugin.toml` manifest plus argv commands in any language — no
//! SDK; the JSON-RPC API socket is the integration surface.
//!
//! Three capabilities per plugin:
//! - `[[on_event]]`: one-shot argv command spawned when a subscribed API
//!   event fires (event JSON on stdin, `{event}` placeholder in args).
//! - `[service]`: a long-running child started with the server, restarted
//!   with capped backoff, killed on shutdown.
//! - `[agent_manifest]`: a bundled agent-detection manifest merged into the
//!   runtime detection registry (built-ins win name collisions).
//!
//! Discovery scans `$XDG_CONFIG_HOME/spectra/plugins/<name>/` first, then
//! `$XDG_DATA_HOME/spectra/plugins/<name>/`; the config dir wins name
//! collisions. Plugin failures (parse errors, spawn failures, crash loops)
//! are contained and logged — they never take the server down.

mod manifest;
mod service;

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

pub use manifest::{OnEventSpec, PluginManifest, sanitize_plugin_name};
pub use service::{SERVICE_LOG_FILE, ServiceTuning};

use crate::agent::AgentManifest;
use service::ServiceSupervisor;

/// Manifest file name inside each plugin directory.
pub const MANIFEST_FILE: &str = "spectra-plugin.toml";

/// Shared log sink for plugin load reports and background-thread failures.
pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// One successfully loaded plugin.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    /// Plugin directory (cwd of every spawned command).
    pub dir: PathBuf,
    /// Bundled agent-detection manifest, parsed at load time.
    pub agent_manifest: Option<AgentManifest>,
}

/// Owns plugin discovery, event dispatch, and service supervision. The rest
/// of the app talks to it through this narrow interface; it stays inactive
/// (a no-op) until [`PluginHost::load`] runs, so client/CLI modes never
/// scan directories or spawn services.
pub struct PluginHost {
    active: bool,
    plugins: Vec<LoadedPlugin>,
    services: HashMap<String, ServiceSupervisor>,
    scan_dirs: Vec<PathBuf>,
    envs: Vec<(String, String)>,
    logger: Logger,
    tuning: ServiceTuning,
}

impl PluginHost {
    /// Inactive host: every method is a no-op until `load` is called.
    pub fn new() -> Self {
        Self {
            active: false,
            plugins: Vec::new(),
            services: HashMap::new(),
            scan_dirs: Vec::new(),
            envs: Vec::new(),
            logger: Arc::new(|_line: &str| {}),
            tuning: ServiceTuning::default(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Activate the host and perform the initial scan. `scan_dirs` are
    /// searched in order; the first directory containing a given plugin name
    /// wins. `envs` is added to every spawned command's environment
    /// (`SPECTRA_API_SOCKET` in production). Returns log lines describing
    /// what was loaded and what was skipped.
    pub fn load(
        &mut self,
        scan_dirs: Vec<PathBuf>,
        envs: Vec<(String, String)>,
        logger: Logger,
        tuning: ServiceTuning,
    ) -> Vec<String> {
        self.active = true;
        self.scan_dirs = scan_dirs;
        self.envs = envs;
        self.logger = logger;
        self.tuning = tuning;
        self.rescan()
    }

    /// Re-scan the plugin directories (config reload): manifests are
    /// re-parsed, removed plugins' services are killed, added plugins'
    /// services are started, and unchanged services keep running.
    pub fn rescan(&mut self) -> Vec<String> {
        if !self.active {
            return Vec::new();
        }
        let mut report = Vec::new();
        let next = self.scan(&mut report);

        // Keep services whose plugin still exists with an unchanged argv;
        // dropping a supervisor kills its child.
        let mut kept = HashMap::new();
        for (name, supervisor) in self.services.drain() {
            let unchanged = next.iter().any(|plugin| {
                plugin.manifest.name == name
                    && plugin.manifest.service.as_deref() == Some(supervisor.command())
            });
            if unchanged {
                kept.insert(name, supervisor);
            } else {
                report.push(format!("plugin {name}: service stopped"));
            }
        }
        self.services = kept;

        for plugin in &next {
            let Some(argv) = plugin.manifest.service.as_deref() else {
                continue;
            };
            if self.services.contains_key(&plugin.manifest.name) {
                continue;
            }
            match ServiceSupervisor::start(
                &plugin.manifest.name,
                &plugin.dir,
                argv,
                self.envs.clone(),
                Arc::clone(&self.logger),
                self.tuning,
            ) {
                Ok(supervisor) => {
                    self.services
                        .insert(plugin.manifest.name.clone(), supervisor);
                    report.push(format!("plugin {}: service started", plugin.manifest.name));
                }
                Err(err) => {
                    report.push(format!(
                        "plugin {}: service not started: {err}",
                        plugin.manifest.name
                    ));
                }
            }
        }

        self.plugins = next;
        report
    }

    /// Spawn every matching `[[on_event]]` command for one API event
    /// (fire-and-forget, detached threads, failures logged). The event JSON
    /// line is written to each command's stdin.
    pub fn dispatch_event(&self, event: &crate::api::ApiEvent) {
        if !self.active {
            return;
        }
        let mut stdin_line: Option<String> = None;
        for plugin in &self.plugins {
            for spec in &plugin.manifest.on_event {
                if !spec.events.iter().any(|name| name == &event.name) {
                    continue;
                }
                let line = stdin_line
                    .get_or_insert_with(|| format!("{}\n", event.event_line()))
                    .clone();
                let argv: Vec<String> = spec
                    .command
                    .iter()
                    .map(|arg| arg.replace("{event}", &event.name))
                    .collect();
                let mut envs = self.envs.clone();
                envs.push(("SPECTRA_EVENT".to_string(), event.name.clone()));
                spawn_event_command(
                    plugin.manifest.name.clone(),
                    plugin.dir.clone(),
                    argv,
                    envs,
                    line,
                    Arc::clone(&self.logger),
                );
            }
        }
    }

    /// Scan the configured directories, earliest directory winning name
    /// collisions. Invalid manifests are skipped with a report line.
    fn scan(&self, report: &mut Vec<String>) -> Vec<LoadedPlugin> {
        let mut loaded: Vec<LoadedPlugin> = Vec::new();
        for scan_dir in &self.scan_dirs {
            let Ok(entries) = fs::read_dir(scan_dir) else {
                continue;
            };
            let mut plugin_dirs: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect();
            plugin_dirs.sort();
            for plugin_dir in plugin_dirs {
                load_plugin_dir(&plugin_dir, &mut loaded, report);
            }
        }
        loaded
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Load one plugin directory into `loaded`, reporting skips and successes.
fn load_plugin_dir(plugin_dir: &Path, loaded: &mut Vec<LoadedPlugin>, report: &mut Vec<String>) {
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        return;
    }
    let display = plugin_dir.display();
    let text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(err) => {
            report.push(format!(
                "plugin at {display} skipped: manifest read failed: {err}"
            ));
            return;
        }
    };
    let manifest = match PluginManifest::parse(&text) {
        Ok(manifest) => manifest,
        Err(err) => {
            report.push(format!("plugin at {display} skipped: {err}"));
            return;
        }
    };
    let dir_name = plugin_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if manifest.name != dir_name {
        report.push(format!(
            "plugin at {display} skipped: manifest name {:?} does not match directory name {dir_name:?}",
            manifest.name
        ));
        return;
    }
    if loaded
        .iter()
        .any(|plugin| plugin.manifest.name == manifest.name)
    {
        report.push(format!(
            "plugin at {display} skipped: shadowed by an earlier directory with the same name"
        ));
        return;
    }

    // A broken bundled agent manifest disables only that capability; the
    // rest of the plugin still loads.
    let agent_manifest = manifest.agent_manifest_path.as_ref().and_then(|relative| {
        let path = plugin_dir.join(relative);
        let parsed = fs::read_to_string(&path)
            .map_err(|err| format!("read failed: {err}"))
            .and_then(|text| AgentManifest::parse(&text));
        match parsed {
            Ok(agent_manifest) => Some(agent_manifest),
            Err(err) => {
                report.push(format!(
                    "plugin {}: agent manifest {relative:?} ignored: {err}",
                    manifest.name
                ));
                None
            }
        }
    });

    report.push(format!("plugin {} loaded from {display}", manifest.name));
    loaded.push(LoadedPlugin {
        manifest,
        dir: plugin_dir.to_path_buf(),
        agent_manifest,
    });
}

/// Spawn one on_event command on a detached thread, mirroring the hook
/// runner's fire-and-forget style, but exec'ing the argv directly (no shell).
fn spawn_event_command(
    plugin: String,
    dir: PathBuf,
    argv: Vec<String>,
    envs: Vec<(String, String)>,
    stdin_line: String,
    logger: Logger,
) {
    let thread_plugin = plugin.clone();
    let thread_logger = Arc::clone(&logger);
    let spawn_result = thread::Builder::new()
        .name("spectra-plugin-event".to_string())
        .spawn(move || {
            let plugin = thread_plugin;
            let logger = thread_logger;
            let Some(program) = argv.first() else {
                return;
            };
            let mut child = match Command::new(program)
                .args(&argv[1..])
                .current_dir(&dir)
                .envs(envs)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    logger(&format!(
                        "plugin {plugin}: on_event command {program:?} failed to spawn: {err}"
                    ));
                    return;
                }
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(stdin_line.as_bytes());
            }
            match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    logger(&format!(
                        "plugin {plugin}: on_event command failed: {status}"
                    ));
                }
                Err(err) => {
                    logger(&format!(
                        "plugin {plugin}: on_event command wait failed: {err}"
                    ));
                }
            }
        });
    if let Err(err) = spawn_result {
        logger(&format!(
            "plugin {plugin}: on_event thread spawn failed: {err}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn noop_logger() -> Logger {
        Arc::new(|_line: &str| {})
    }

    fn write_plugin(base: &Path, name: &str, manifest: &str) -> PathBuf {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("create plugin dir");
        fs::write(dir.join(MANIFEST_FILE), manifest).expect("write manifest");
        dir
    }

    fn load_host(scan_dirs: Vec<PathBuf>) -> (PluginHost, Vec<String>) {
        let mut host = PluginHost::new();
        let report = host.load(
            scan_dirs,
            vec![(
                "SPECTRA_API_SOCKET".to_string(),
                "/tmp/test-api.sock".to_string(),
            )],
            noop_logger(),
            ServiceTuning::default(),
        );
        (host, report)
    }

    fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn discovery_loads_valid_and_skips_invalid_manifests() {
        let base = tempfile::tempdir().expect("tempdir");
        write_plugin(base.path(), "good", "name = \"good\"\ndescription = \"ok\"");
        write_plugin(base.path(), "broken", "name = \"broken\"\nbogus = true");
        write_plugin(base.path(), "renamed", "name = \"other\"");
        // A directory without a manifest is silently ignored.
        fs::create_dir_all(base.path().join("empty")).expect("create empty dir");

        let (host, report) = load_host(vec![base.path().to_path_buf()]);

        assert_eq!(host.plugins().len(), 1);
        assert_eq!(host.plugins()[0].manifest.name, "good");
        assert!(
            report
                .iter()
                .any(|line| line.contains("broken") && line.contains("skipped"))
        );
        assert!(
            report
                .iter()
                .any(|line| line.contains("does not match directory name"))
        );
    }

    #[test]
    fn earlier_scan_dir_takes_precedence_on_name_collision() {
        let config = tempfile::tempdir().expect("tempdir");
        let data = tempfile::tempdir().expect("tempdir");
        write_plugin(
            config.path(),
            "dupe",
            "name = \"dupe\"\ndescription = \"from config\"",
        );
        write_plugin(
            data.path(),
            "dupe",
            "name = \"dupe\"\ndescription = \"from data\"",
        );

        let (host, report) =
            load_host(vec![config.path().to_path_buf(), data.path().to_path_buf()]);

        assert_eq!(host.plugins().len(), 1);
        assert_eq!(host.plugins()[0].manifest.description, "from config");
        assert!(report.iter().any(|line| line.contains("shadowed")));
    }

    #[test]
    fn bundled_agent_manifest_is_parsed_and_bad_one_is_ignored() {
        let base = tempfile::tempdir().expect("tempdir");
        let good = write_plugin(
            base.path(),
            "witha",
            "name = \"witha\"\n[agent_manifest]\npath = \"agent.toml\"",
        );
        fs::write(
            good.join("agent.toml"),
            "name = \"custombot\"\ntitle_markers = [\"custombot\"]",
        )
        .expect("write agent manifest");
        let bad = write_plugin(
            base.path(),
            "withbad",
            "name = \"withbad\"\n[agent_manifest]\npath = \"agent.toml\"",
        );
        fs::write(bad.join("agent.toml"), "not valid toml [[[").expect("write bad agent manifest");

        let (host, report) = load_host(vec![base.path().to_path_buf()]);

        assert_eq!(host.plugins().len(), 2);
        let with_agent = host
            .plugins()
            .iter()
            .find(|plugin| plugin.manifest.name == "witha")
            .expect("witha plugin");
        assert_eq!(
            with_agent.agent_manifest.as_ref().map(AgentManifest::name),
            Some("custombot")
        );
        let with_bad = host
            .plugins()
            .iter()
            .find(|plugin| plugin.manifest.name == "withbad")
            .expect("withbad plugin");
        assert!(with_bad.agent_manifest.is_none());
        assert!(
            report
                .iter()
                .any(|line| line.contains("agent manifest") && line.contains("ignored"))
        );
    }

    #[test]
    fn dispatch_event_runs_command_with_stdin_placeholder_and_env() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = write_plugin(
            base.path(),
            "notify",
            r#"
name = "notify"

[[on_event]]
events = ["agent.changed"]
command = ["/bin/sh", "handler.sh", "{event}"]
"#,
        );
        fs::write(
            dir.join("handler.sh"),
            "cat > stdin.json\nprintf '%s' \"$1\" > arg.txt\nprintf '%s' \"$SPECTRA_EVENT\" > event.txt\nprintf '%s' \"$SPECTRA_API_SOCKET\" > socket.txt\n",
        )
        .expect("write handler");

        let (host, _report) = load_host(vec![base.path().to_path_buf()]);
        host.dispatch_event(&crate::api::ApiEvent {
            name: "agent.changed".to_string(),
            params: serde_json::json!({"pane_id": 3, "state": "idle"}),
        });

        assert!(wait_until(Duration::from_secs(5), || {
            dir.join("socket.txt").exists()
                && dir.join("stdin.json").exists()
                && dir.join("arg.txt").exists()
        }));
        let stdin_json = fs::read_to_string(dir.join("stdin.json")).expect("stdin capture");
        let parsed: serde_json::Value =
            serde_json::from_str(stdin_json.trim()).expect("stdin is one JSON line");
        assert_eq!(parsed["event"], "agent.changed");
        assert_eq!(parsed["params"]["pane_id"], 3);
        assert_eq!(
            fs::read_to_string(dir.join("arg.txt")).expect("arg capture"),
            "agent.changed"
        );
        assert_eq!(
            fs::read_to_string(dir.join("event.txt")).expect("env capture"),
            "agent.changed"
        );
        assert_eq!(
            fs::read_to_string(dir.join("socket.txt")).expect("socket capture"),
            "/tmp/test-api.sock"
        );
    }

    #[test]
    fn dispatch_event_skips_unsubscribed_events_and_inactive_host() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = write_plugin(
            base.path(),
            "picky",
            r#"
name = "picky"

[[on_event]]
events = ["pane.closed"]
command = ["/bin/sh", "-c", "touch fired.txt"]
"#,
        );

        // Inactive host: no-op even for a matching event.
        let inactive = PluginHost::new();
        inactive.dispatch_event(&crate::api::ApiEvent {
            name: "pane.closed".to_string(),
            params: serde_json::Value::Null,
        });

        let (host, _report) = load_host(vec![base.path().to_path_buf()]);
        host.dispatch_event(&crate::api::ApiEvent {
            name: "agent.changed".to_string(),
            params: serde_json::Value::Null,
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(!dir.join("fired.txt").exists());

        host.dispatch_event(&crate::api::ApiEvent {
            name: "pane.closed".to_string(),
            params: serde_json::Value::Null,
        });
        assert!(wait_until(Duration::from_secs(5), || {
            dir.join("fired.txt").exists()
        }));
    }

    #[test]
    fn rescan_starts_new_services_and_stops_removed_ones() {
        let base = tempfile::tempdir().expect("tempdir");
        let svc_dir = write_plugin(
            base.path(),
            "daemon",
            r#"
name = "daemon"

[service]
command = ["/bin/sh", "-c", "touch started.txt; sleep 30"]
"#,
        );

        let mut host = PluginHost::new();
        let report = host.load(
            vec![base.path().to_path_buf()],
            Vec::new(),
            noop_logger(),
            crate::plugin::ServiceTuning {
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(20),
                poll_interval: Duration::from_millis(5),
                ..ServiceTuning::default()
            },
        );
        assert!(report.iter().any(|line| line.contains("service started")));
        assert!(wait_until(Duration::from_secs(5), || {
            svc_dir.join("started.txt").exists()
        }));
        assert_eq!(host.services.len(), 1);

        // Removing the plugin directory kills the service on rescan.
        fs::remove_dir_all(&svc_dir).expect("remove plugin dir");
        let report = host.rescan();
        assert!(report.iter().any(|line| line.contains("service stopped")));
        assert!(host.plugins().is_empty());
        assert!(host.services.is_empty());
    }
}
