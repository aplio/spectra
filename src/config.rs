use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub prefix: Option<String>,
    #[serde(default = "default_true")]
    pub prefix_sticky: bool,
    pub session_name: Option<String>,
    pub initial_command: Option<String>,
    pub editor: Option<String>,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub mouse: MouseConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub pane: PaneConfig,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub sidebar: SidebarConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub prefix_bindings: HashMap<String, String>,
    #[serde(default)]
    pub global_bindings: HashMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        // Mirrors the serde field defaults so a missing config file and a
        // config that omits fields behave identically. In particular
        // `prefix_sticky` defaults to true (see `default_true`), which a
        // derived `Default` would get wrong.
        Self {
            prefix: None,
            prefix_sticky: default_true(),
            session_name: None,
            initial_command: None,
            editor: None,
            shell: ShellConfig::default(),
            mouse: MouseConfig::default(),
            terminal: TerminalConfig::default(),
            pane: PaneConfig::default(),
            status: StatusConfig::default(),
            agent: AgentConfig::default(),
            sidebar: SidebarConfig::default(),
            hooks: HooksConfig::default(),
            prefix_bindings: HashMap::new(),
            global_bindings: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShellConfig {
    #[serde(default = "default_true")]
    pub suppress_prompt_eol_marker: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            suppress_prompt_eol_marker: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalConfig {
    #[serde(default = "default_true")]
    pub allow_passthrough: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            allow_passthrough: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaneConfig {
    /// Seconds a user-closed pane is kept alive so `restore-pane` can undo
    /// the close (ghostty-style undo close). 0 disables retention.
    #[serde(default = "default_undo_close_seconds")]
    pub undo_close_seconds: u64,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            undo_close_seconds: default_undo_close_seconds(),
        }
    }
}

fn default_undo_close_seconds() -> u64 {
    10
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MouseConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StatusConfig {
    pub format: Option<String>,
    pub background: Option<String>,
    pub foreground: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub notify: AgentNotifyMode,
}

/// When to send an OSC 9 desktop notification to attached clients on an
/// agent state change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentNotifyMode {
    /// Never notify.
    Off,
    /// Notify only when a pane's agent enters `blocked`.
    #[default]
    Blocked,
    /// Also notify when a pane's agent becomes `done` (unseen idle).
    All,
}

/// Behavior of the window-tree sidebar (the "side window tree" overlay).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SidebarConfig {
    /// Whether the sidebar is shown by default on session start.
    #[serde(default = "default_true")]
    pub default_open: bool,
    /// Format of session header rows, with `{token}` placeholders like
    /// `[status].format`. A `\n` splits the row into multiple lines.
    pub session_format: Option<String>,
    /// Format of window rows, with `{token}` placeholders like
    /// `[status].format`. A `\n` splits the row into multiple lines.
    pub window_format: Option<String>,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            default_open: true,
            session_format: None,
            window_format: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksConfig {
    pub session_created: Option<String>,
    pub session_killed: Option<String>,
    pub window_created: Option<String>,
    pub pane_split: Option<String>,
    pub pane_closed: Option<String>,
    pub pane_restored: Option<String>,
    pub config_reloaded: Option<String>,
}

pub fn config_path() -> PathBuf {
    crate::xdg::app_config_dir().join("config.toml")
}

pub fn load_from_xdg() -> io::Result<AppConfig> {
    load_from_path(&config_path())
}

pub fn load_from_path(path: &Path) -> io::Result<AppConfig> {
    load_toml_with_default(path)
}

fn load_toml_with_default<T>(path: &Path) -> io::Result<T>
where
    T: DeserializeOwned + Default,
{
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(err) => return Err(err),
    };

    toml::from_str::<T>(&content).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed parsing config {}: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::load_from_path;

    #[test]
    fn missing_config_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        let config = load_from_path(&path).expect("load missing");
        assert!(config.prefix.is_none());
        assert!(config.prefix_sticky);
        assert!(config.initial_command.is_none());
        assert!(config.editor.is_none());
        assert!(config.session_name.is_none());
        assert!(config.shell.suppress_prompt_eol_marker);
        assert!(!config.mouse.enabled);
        assert!(config.terminal.allow_passthrough);
        assert!(config.status.format.is_none());
        assert!(config.status.background.is_none());
        assert!(config.status.foreground.is_none());
        assert_eq!(config.agent.notify, super::AgentNotifyMode::Blocked);
        assert!(config.sidebar.default_open);
        assert!(config.sidebar.session_format.is_none());
        assert!(config.sidebar.window_format.is_none());
        assert!(config.hooks.session_created.is_none());
        assert!(config.prefix_bindings.is_empty());
        assert!(config.global_bindings.is_empty());
    }

    #[test]
    fn parses_config_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r##"
prefix = "C-a"
session_name = "dev"
initial_command = "echo hi"
editor = "hx"

[shell]
suppress_prompt_eol_marker = true

[mouse]
enabled = true

[terminal]
allow_passthrough = false

[status]
format = "session {session_index}"
background = "#2E3440"
foreground = "#D8DEE9"

[agent]
notify = "all"

[sidebar]
default_open = false
session_format = "{session_index}:{session_name}"
window_format = "w{window_index} {window_name}\n  {agent}"

[hooks]
session_created = "echo created"
config_reloaded = "echo reloaded"

[prefix_bindings]
w = "window-list"

[global_bindings]
C-w = "window-list"
"##,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("load config");
        assert_eq!(config.prefix.as_deref(), Some("C-a"));
        assert_eq!(config.session_name.as_deref(), Some("dev"));
        assert_eq!(config.initial_command.as_deref(), Some("echo hi"));
        assert_eq!(config.editor.as_deref(), Some("hx"));
        assert!(config.shell.suppress_prompt_eol_marker);
        assert!(config.mouse.enabled);
        assert!(!config.terminal.allow_passthrough);
        assert_eq!(
            config.status.format.as_deref(),
            Some("session {session_index}")
        );
        assert_eq!(config.status.background.as_deref(), Some("#2E3440"));
        assert_eq!(config.status.foreground.as_deref(), Some("#D8DEE9"));
        assert_eq!(config.agent.notify, super::AgentNotifyMode::All);
        assert!(!config.sidebar.default_open);
        assert_eq!(
            config.sidebar.session_format.as_deref(),
            Some("{session_index}:{session_name}")
        );
        assert_eq!(
            config.sidebar.window_format.as_deref(),
            Some("w{window_index} {window_name}\n  {agent}")
        );
        assert_eq!(
            config.hooks.session_created.as_deref(),
            Some("echo created")
        );
        assert_eq!(
            config.hooks.config_reloaded.as_deref(),
            Some("echo reloaded")
        );
        assert_eq!(
            config.prefix_bindings.get("w").map(String::as_str),
            Some("window-list")
        );
        assert_eq!(
            config.global_bindings.get("C-w").map(String::as_str),
            Some("window-list")
        );
    }
}
