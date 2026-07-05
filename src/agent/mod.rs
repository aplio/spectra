//! AI coding-agent state detection (herdr-inspired).
//!
//! Detection is manifest-driven: per-agent TOML manifests declare prioritized
//! rules that are matched against the pane's visible screen bottom lines and
//! OSC title, plus a Linux best-effort foreground-process name match.

mod manifest;
mod proc;

use std::sync::OnceLock;
use std::time::Instant;

pub use manifest::AgentManifest;
pub use proc::foreground_process_name;

/// Coarse state of an AI coding agent running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Agent presence detected but no state rule matched.
    Unknown,
    /// Input prompt at rest, waiting for the user.
    Idle,
    /// Agent is running a turn.
    Working,
    /// Agent is waiting on a permission/approval prompt.
    Blocked,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// Detection result for one pane.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    /// Manifest name, e.g. `"claude"`.
    pub kind: String,
    pub state: AgentState,
    /// When the pane entered the current `(kind, state)`.
    pub since: Instant,
}

/// Read-only view of one pane's detection inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneSnapshot<'a> {
    /// Visible screen rows, top to bottom (not scrollback-scrolled viewport).
    pub screen_lines: &'a [String],
    /// Latest OSC 0/2 title, if any.
    pub osc_title: Option<&'a str>,
    /// Foreground process argv[0] basename, if resolvable.
    pub foreground_process: Option<&'a str>,
}

/// Run detection over `manifests` and return the first agent whose presence
/// is established, with its detected state.
///
/// Presence = any state rule matched, or the OSC title contains a
/// `title_markers` entry, or the foreground process name is listed in
/// `process_names`. Presence without a matching rule yields
/// [`AgentState::Unknown`].
pub fn detect(
    manifests: &[AgentManifest],
    snapshot: &PaneSnapshot<'_>,
) -> Option<(String, AgentState)> {
    manifests
        .iter()
        .find_map(|manifest| Some((manifest.name().to_string(), manifest.detect(snapshot)?)))
}

static BUILTIN_MANIFESTS: OnceLock<Vec<AgentManifest>> = OnceLock::new();

/// Built-in agent manifests, parsed once on first use.
pub fn builtin_manifests() -> &'static [AgentManifest] {
    BUILTIN_MANIFESTS.get_or_init(|| {
        vec![
            AgentManifest::parse(include_str!("manifests/claude.toml"))
                .expect("embedded claude manifest is valid"),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    fn detect_screen(rows: &[&str]) -> Option<(String, AgentState)> {
        let screen_lines = lines(rows);
        detect(
            builtin_manifests(),
            &PaneSnapshot {
                screen_lines: &screen_lines,
                osc_title: None,
                foreground_process: None,
            },
        )
    }

    #[test]
    fn builtin_claude_manifest_parses() {
        let manifests = builtin_manifests();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name(), "claude");
        assert_eq!(manifests[0].display_name(), "Claude Code");
    }

    #[test]
    fn claude_working_screen_detected() {
        let detected = detect_screen(&[
            "some earlier output",
            "",
            "✳ Compacting… (esc to interrupt)",
            "╭──────────────────────────╮",
            "│ >                        │",
            "╰──────────────────────────╯",
        ]);
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Working)));
    }

    #[test]
    fn claude_blocked_screen_detected() {
        let detected = detect_screen(&[
            "╭──────────────────────────────────╮",
            "│ Do you want to make this edit?   │",
            "│ ❯ 1. Yes                         │",
            "│   2. Yes, allow all edits        │",
            "│   3. No                          │",
            "╰──────────────────────────────────╯",
        ]);
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Blocked)));
    }

    #[test]
    fn claude_blocked_wins_over_working_by_priority() {
        let detected = detect_screen(&[
            "✳ Compacting… (esc to interrupt)",
            "Do you want to proceed?",
            "❯ 1. Yes",
            "  2. No",
        ]);
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Blocked)));
    }

    #[test]
    fn claude_idle_prompt_screen_detected() {
        let detected = detect_screen(&[
            "response text from the previous turn",
            "",
            "╭──────────────────────────╮",
            "│ >                        │",
            "╰──────────────────────────╯",
            "  ? for shortcuts",
        ]);
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Idle)));
    }

    #[test]
    fn claude_idle_prompt_box_without_shortcuts_hint_detected() {
        let detected = detect_screen(&[
            "╭──────────────────────────╮",
            "│ >                        │",
            "╰──────────────────────────╯",
        ]);
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Idle)));
    }

    #[test]
    fn plain_shell_screen_is_not_detected() {
        let detected =
            detect_screen(&["$ ls", "Cargo.toml src target", "$ echo done", "done", "$ "]);
        assert_eq!(detected, None);
    }

    #[test]
    fn shell_continuation_prompt_is_not_detected() {
        // A bare "> " continuation prompt without Claude's input box must not
        // register as claude.
        let detected = detect_screen(&["$ cat <<EOF", "> hello", "> "]);
        assert_eq!(detected, None);
    }

    #[test]
    fn title_marker_alone_yields_unknown_presence() {
        let screen_lines = lines(&["$ "]);
        let detected = detect(
            builtin_manifests(),
            &PaneSnapshot {
                screen_lines: &screen_lines,
                osc_title: Some("✳ my-project"),
                foreground_process: None,
            },
        );
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Unknown)));
    }

    #[test]
    fn process_name_alone_yields_unknown_presence() {
        let screen_lines = lines(&["$ "]);
        let detected = detect(
            builtin_manifests(),
            &PaneSnapshot {
                screen_lines: &screen_lines,
                osc_title: None,
                foreground_process: Some("claude"),
            },
        );
        assert_eq!(detected, Some(("claude".to_string(), AgentState::Unknown)));
        let not_matching = detect(
            builtin_manifests(),
            &PaneSnapshot {
                screen_lines: &screen_lines,
                osc_title: None,
                foreground_process: Some("vim"),
            },
        );
        assert_eq!(not_matching, None);
    }
}
