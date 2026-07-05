//! App-side plugin lifecycle: the narrow bridge between [`App`] and
//! [`crate::plugin::PluginHost`]. Discovery, event dispatch, and service
//! supervision all live in `crate::plugin`; this module only wires loading,
//! config-reload rescans, the agent-detection registry merge, and the
//! `plugin.list` API view.

use std::path::PathBuf;
use std::sync::Arc;

use super::App;

impl App {
    /// Load plugins from the default XDG locations (config dir takes
    /// precedence over data dir). Called once by the server at startup;
    /// client/CLI modes never call it, so they never spawn services.
    pub fn load_plugins(&mut self) {
        self.load_plugins_from(
            default_plugin_dirs(),
            crate::plugin::ServiceTuning::default(),
        );
    }

    /// Test seam behind [`App::load_plugins`]: explicit scan directories and
    /// service tuning.
    pub(crate) fn load_plugins_from(
        &mut self,
        scan_dirs: Vec<PathBuf>,
        tuning: crate::plugin::ServiceTuning,
    ) {
        // Background plugin threads log through the store into the session
        // log of the session that was current at load time.
        let store = self.store.clone();
        let log_session_id = self.current_session_id().to_string();
        let logger: crate::plugin::Logger = Arc::new(move |line: &str| {
            let _ = store.append_log_line(&log_session_id, line);
        });
        let envs = vec![(
            "SPECTRA_API_SOCKET".to_string(),
            crate::ipc::socket_path::api_socket_path()
                .to_string_lossy()
                .into_owned(),
        )];

        let report = self.plugins.load(scan_dirs, envs, logger, tuning);
        for line in report {
            self.write_log(&line);
        }
        self.refresh_agent_registry();
    }

    /// Re-scan plugin directories after a config reload. No-op unless
    /// [`App::load_plugins`] ran first (i.e. outside the server).
    pub(super) fn reload_plugins(&mut self) {
        if !self.plugins.is_active() {
            return;
        }
        let report = self.plugins.rescan();
        for line in report {
            self.write_log(&line);
        }
        self.refresh_agent_registry();
    }

    /// Rebuild the agent-detection registry: built-in manifests first, then
    /// plugin-provided ones. On a name collision the built-in (or earlier
    /// plugin) wins and the loser is logged.
    fn refresh_agent_registry(&mut self) {
        let mut manifests = crate::agent::parse_builtin_manifests();
        let mut warnings = Vec::new();
        for plugin in self.plugins.plugins() {
            let Some(agent_manifest) = &plugin.agent_manifest else {
                continue;
            };
            if manifests
                .iter()
                .any(|existing| existing.name() == agent_manifest.name())
            {
                warnings.push(format!(
                    "plugin {}: agent manifest {:?} ignored (a built-in or earlier manifest wins the name)",
                    plugin.manifest.name,
                    agent_manifest.name()
                ));
                continue;
            }
            manifests.push(agent_manifest.clone());
        }
        self.agent_manifests = Arc::new(manifests);
        for warning in warnings {
            self.write_log(&warning);
        }
    }

    /// `plugin.list` view of the loaded plugins.
    pub(crate) fn api_plugins(&self) -> Vec<crate::api::PluginInfo> {
        self.plugins
            .plugins()
            .iter()
            .map(|plugin| {
                let mut events: Vec<String> = plugin
                    .manifest
                    .on_event
                    .iter()
                    .flat_map(|spec| spec.events.iter().cloned())
                    .collect();
                events.sort();
                events.dedup();
                crate::api::PluginInfo {
                    name: plugin.manifest.name.clone(),
                    description: plugin.manifest.description.clone(),
                    has_service: plugin.manifest.service.is_some(),
                    on_event_commands: plugin.manifest.on_event.len(),
                    events,
                    agent_manifest: plugin
                        .agent_manifest
                        .as_ref()
                        .map(|manifest| manifest.name().to_string()),
                }
            })
            .collect()
    }

    /// Agent-detection registry (built-ins + plugin manifests); test seam.
    #[cfg(test)]
    pub(super) fn agent_registry(&self) -> &[crate::agent::AgentManifest] {
        &self.agent_manifests
    }
}

/// Default plugin locations: user-authored plugins conventionally live in
/// the config dir; the data dir is supported for tool-installed plugins.
/// The config dir wins name collisions.
fn default_plugin_dirs() -> Vec<PathBuf> {
    vec![
        crate::xdg::app_config_dir().join("plugins"),
        crate::xdg::app_data_dir().join("plugins"),
    ]
}
