//! `spectra-plugin.toml` manifest format.
//!
//! A plugin is a directory containing this manifest plus whatever scripts or
//! binaries its commands reference. All commands are plain argv vectors — no
//! shell is involved — and run with the plugin directory as working
//! directory, so relative paths like `./notify.sh` work.

use serde::Deserialize;

/// Maximum length of a plugin name (same policy as agent kinds).
pub const PLUGIN_NAME_MAX_CHARS: usize = 32;

/// Parsed and validated plugin manifest.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Plugin name; must equal the plugin directory name and consist of
    /// lowercase ASCII alphanumerics and dashes (max 32 chars).
    pub name: String,
    pub description: String,
    /// Event-triggered one-shot commands.
    pub on_event: Vec<OnEventSpec>,
    /// Long-running service argv, supervised by the server.
    pub service: Option<Vec<String>>,
    /// Relative path to a bundled agent-detection manifest.
    pub agent_manifest_path: Option<String>,
}

/// One `[[on_event]]` entry: spawn `command` when any of `events` fires.
#[derive(Debug, Clone)]
pub struct OnEventSpec {
    /// API event names (e.g. `"agent.changed"`); unknown names are kept for
    /// forward compatibility but never fire today.
    pub events: Vec<String>,
    /// Argv to spawn; `{event}` in any element is replaced by the event name.
    pub command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    on_event: Vec<RawOnEvent>,
    #[serde(default)]
    service: Option<RawService>,
    #[serde(default)]
    agent_manifest: Option<RawAgentManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOnEvent {
    events: Vec<String>,
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentManifest {
    path: String,
}

/// Normalize a plugin name the same way externally reported agent kinds are
/// normalized: lowercase, ASCII alphanumerics and dashes only, capped length.
pub fn sanitize_plugin_name(raw: &str) -> String {
    raw.chars()
        .filter_map(|character| {
            let character = character.to_ascii_lowercase();
            (character.is_ascii_alphanumeric() || character == '-').then_some(character)
        })
        .take(PLUGIN_NAME_MAX_CHARS)
        .collect()
}

impl PluginManifest {
    /// Parse and validate a manifest from TOML.
    pub fn parse(toml_text: &str) -> Result<Self, String> {
        let raw: RawManifest =
            toml::from_str(toml_text).map_err(|err| format!("manifest parse error: {err}"))?;

        if raw.name.is_empty() || sanitize_plugin_name(&raw.name) != raw.name {
            return Err(format!(
                "name {:?} must be 1-{PLUGIN_NAME_MAX_CHARS} lowercase ASCII alphanumeric/dash characters",
                raw.name
            ));
        }

        let mut on_event = Vec::with_capacity(raw.on_event.len());
        for (index, entry) in raw.on_event.into_iter().enumerate() {
            let context = format!("on_event #{}", index + 1);
            if entry.events.is_empty() || entry.events.iter().any(String::is_empty) {
                return Err(format!("{context}: events must be non-empty strings"));
            }
            validate_argv(&entry.command, &context)?;
            on_event.push(OnEventSpec {
                events: entry.events,
                command: entry.command,
            });
        }

        let service = match raw.service {
            Some(service) => {
                validate_argv(&service.command, "service")?;
                Some(service.command)
            }
            None => None,
        };

        let agent_manifest_path = match raw.agent_manifest {
            Some(agent_manifest) => {
                if agent_manifest.path.is_empty() {
                    return Err("agent_manifest.path must not be empty".to_string());
                }
                if std::path::Path::new(&agent_manifest.path).is_absolute() {
                    return Err(format!(
                        "agent_manifest.path {:?} must be relative to the plugin directory",
                        agent_manifest.path
                    ));
                }
                Some(agent_manifest.path)
            }
            None => None,
        };

        Ok(Self {
            name: raw.name,
            description: raw.description.unwrap_or_default(),
            on_event,
            service,
            agent_manifest_path,
        })
    }
}

fn validate_argv(command: &[String], context: &str) -> Result<(), String> {
    match command.first() {
        None => Err(format!("{context}: command must be a non-empty argv array")),
        Some(program) if program.is_empty() => {
            Err(format!("{context}: command program must not be empty"))
        }
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_manifest_parses() {
        let manifest = PluginManifest::parse(
            r#"
name = "my-plugin"
description = "does things"

[[on_event]]
events = ["agent.changed", "pane.closed"]
command = ["./notify.sh", "{event}"]

[[on_event]]
events = ["session.created"]
command = ["python3", "hello.py"]

[service]
command = ["./daemon.py", "--verbose"]

[agent_manifest]
path = "agent.toml"
"#,
        )
        .expect("parse full manifest");

        assert_eq!(manifest.name, "my-plugin");
        assert_eq!(manifest.description, "does things");
        assert_eq!(manifest.on_event.len(), 2);
        assert_eq!(
            manifest.on_event[0].events,
            vec!["agent.changed", "pane.closed"]
        );
        assert_eq!(manifest.on_event[0].command, vec!["./notify.sh", "{event}"]);
        assert_eq!(
            manifest.service.as_deref(),
            Some(["./daemon.py".to_string(), "--verbose".to_string()].as_slice())
        );
        assert_eq!(manifest.agent_manifest_path.as_deref(), Some("agent.toml"));
    }

    #[test]
    fn minimal_manifest_defaults() {
        let manifest = PluginManifest::parse("name = \"tiny\"").expect("parse minimal manifest");
        assert_eq!(manifest.name, "tiny");
        assert_eq!(manifest.description, "");
        assert!(manifest.on_event.is_empty());
        assert!(manifest.service.is_none());
        assert!(manifest.agent_manifest_path.is_none());
    }

    #[test]
    fn unsanitized_or_empty_name_rejected() {
        for name in ["My Plugin", "", "UPPER", "spaced name"] {
            let err = PluginManifest::parse(&format!("name = {name:?}"))
                .expect_err("bad name must be rejected");
            assert!(err.contains("lowercase"), "unexpected error: {err}");
        }
    }

    #[test]
    fn sanitize_matches_agent_kind_policy() {
        assert_eq!(sanitize_plugin_name("My Plugin!"), "myplugin");
        assert_eq!(sanitize_plugin_name("a-b-c"), "a-b-c");
        assert_eq!(sanitize_plugin_name(&"x".repeat(50)).len(), 32);
    }

    #[test]
    fn empty_on_event_command_rejected() {
        let err = PluginManifest::parse(
            r#"
name = "p"

[[on_event]]
events = ["pane.closed"]
command = []
"#,
        )
        .expect_err("empty argv must be rejected");
        assert!(err.contains("non-empty argv"), "unexpected error: {err}");
    }

    #[test]
    fn empty_events_list_rejected() {
        let err = PluginManifest::parse(
            r#"
name = "p"

[[on_event]]
events = []
command = ["./run.sh"]
"#,
        )
        .expect_err("empty events must be rejected");
        assert!(err.contains("events"), "unexpected error: {err}");
    }

    #[test]
    fn empty_service_command_rejected() {
        let err = PluginManifest::parse(
            r#"
name = "p"

[service]
command = [""]
"#,
        )
        .expect_err("empty program must be rejected");
        assert!(err.contains("program"), "unexpected error: {err}");
    }

    #[test]
    fn unknown_field_rejected() {
        let err = PluginManifest::parse("name = \"p\"\nbogus = 1")
            .expect_err("unknown field must be rejected");
        assert!(err.contains("bogus"), "unexpected error: {err}");
    }

    #[test]
    fn absolute_agent_manifest_path_rejected() {
        let err = PluginManifest::parse(
            r#"
name = "p"

[agent_manifest]
path = "/etc/agent.toml"
"#,
        )
        .expect_err("absolute path must be rejected");
        assert!(err.contains("relative"), "unexpected error: {err}");
    }
}
