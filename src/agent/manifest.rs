//! TOML manifest format and rule engine for agent detection.

use regex::Regex;
use serde::Deserialize;

use super::{AgentState, PaneSnapshot};

/// One agent's detection manifest: identity markers plus prioritized
/// screen-matching rules.
#[derive(Debug)]
pub struct AgentManifest {
    name: String,
    display_name: String,
    process_names: Vec<String>,
    title_markers: Vec<String>,
    /// Sorted by descending priority; ties keep declaration order.
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    state: AgentState,
    region: Region,
    contains: Option<String>,
    regex: Option<Regex>,
    any: Option<Vec<String>>,
    all: Option<Vec<String>>,
    not: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Region {
    /// The last N non-blank visible screen lines, joined with `\n`.
    BottomNonEmptyLines(usize),
    /// The pane's latest OSC title.
    OscTitle,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    process_names: Vec<String>,
    #[serde(default)]
    title_markers: Vec<String>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    #[serde(default)]
    priority: i64,
    state: String,
    region: RawRegion,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    any: Option<Vec<String>>,
    #[serde(default)]
    all: Option<Vec<String>>,
    #[serde(default)]
    not: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRegion {
    Named(String),
    Bottom { bottom_non_empty_lines: usize },
}

impl AgentManifest {
    /// Parse and validate a manifest from TOML. Regexes are compiled here,
    /// once per manifest load.
    pub fn parse(toml_text: &str) -> Result<Self, String> {
        let raw: RawManifest =
            toml::from_str(toml_text).map_err(|err| format!("manifest parse error: {err}"))?;

        let mut indexed_rules = Vec::with_capacity(raw.rules.len());
        for (index, raw_rule) in raw.rules.into_iter().enumerate() {
            let rule = Rule::from_raw(raw_rule)
                .map_err(|err| format!("manifest {:?} rule #{}: {err}", raw.name, index + 1))?;
            indexed_rules.push(rule);
        }
        // Stable sort: equal priorities keep declaration order.
        indexed_rules.sort_by_key(|(priority, _)| std::cmp::Reverse(*priority));
        let rules = indexed_rules.into_iter().map(|(_, rule)| rule).collect();

        Ok(Self {
            display_name: raw.display_name.unwrap_or_else(|| raw.name.clone()),
            name: raw.name,
            process_names: raw.process_names,
            title_markers: raw.title_markers,
            rules,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Detect this agent in the pane snapshot. Returns `None` when the agent
    /// is not present at all.
    pub fn detect(&self, snapshot: &PaneSnapshot<'_>) -> Option<AgentState> {
        if let Some(state) = self.match_rules(snapshot) {
            return Some(state);
        }
        if self.matches_title(snapshot.osc_title)
            || self.matches_process(snapshot.foreground_process)
        {
            return Some(AgentState::Unknown);
        }
        None
    }

    /// Highest-priority matching rule wins; ties broken by declaration order.
    fn match_rules(&self, snapshot: &PaneSnapshot<'_>) -> Option<AgentState> {
        if self.rules.is_empty() {
            return None;
        }
        let non_empty_lines: Vec<&str> = snapshot
            .screen_lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.trim().is_empty())
            .collect();
        self.rules.iter().find_map(|rule| {
            let text = rule.region.text(&non_empty_lines, snapshot.osc_title)?;
            rule.matches(&text).then_some(rule.state)
        })
    }

    fn matches_title(&self, osc_title: Option<&str>) -> bool {
        let Some(title) = osc_title else {
            return false;
        };
        self.title_markers
            .iter()
            .any(|marker| title.contains(marker))
    }

    fn matches_process(&self, foreground_process: Option<&str>) -> bool {
        let Some(process) = foreground_process else {
            return false;
        };
        self.process_names.iter().any(|name| name == process)
    }
}

impl Rule {
    fn from_raw(raw: RawRule) -> Result<(i64, Self), String> {
        let state = match raw.state.as_str() {
            "unknown" => AgentState::Unknown,
            "idle" => AgentState::Idle,
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            other => return Err(format!("unknown state {other:?}")),
        };
        let region = match raw.region {
            RawRegion::Named(name) if name == "osc_title" => Region::OscTitle,
            RawRegion::Named(name) => return Err(format!("unknown region {name:?}")),
            RawRegion::Bottom {
                bottom_non_empty_lines,
            } => {
                if bottom_non_empty_lines == 0 {
                    return Err("bottom_non_empty_lines must be at least 1".to_string());
                }
                Region::BottomNonEmptyLines(bottom_non_empty_lines)
            }
        };
        let regex = raw
            .regex
            .map(|pattern| Regex::new(&pattern).map_err(|err| format!("invalid regex: {err}")))
            .transpose()?;

        let has_gate = raw.contains.is_some()
            || regex.is_some()
            || raw.any.as_ref().is_some_and(|list| !list.is_empty())
            || raw.all.as_ref().is_some_and(|list| !list.is_empty())
            || raw.not.is_some();
        if !has_gate {
            return Err("rule needs at least one gate (contains/regex/any/all/not)".to_string());
        }

        Ok((
            raw.priority,
            Self {
                state,
                region,
                contains: raw.contains,
                regex,
                any: raw.any,
                all: raw.all,
                not: raw.not,
            },
        ))
    }

    /// All present gates must pass (AND); `any` is an OR over its entries.
    fn matches(&self, text: &str) -> bool {
        if let Some(needle) = &self.contains
            && !text.contains(needle)
        {
            return false;
        }
        if let Some(regex) = &self.regex
            && !regex.is_match(text)
        {
            return false;
        }
        if let Some(any) = &self.any
            && !any.iter().any(|needle| text.contains(needle))
        {
            return false;
        }
        if let Some(all) = &self.all
            && !all.iter().all(|needle| text.contains(needle))
        {
            return false;
        }
        if let Some(absent) = &self.not
            && text.contains(absent)
        {
            return false;
        }
        true
    }
}

impl Region {
    fn text(self, non_empty_lines: &[&str], osc_title: Option<&str>) -> Option<String> {
        match self {
            Self::BottomNonEmptyLines(max_lines) => {
                let start = non_empty_lines.len().saturating_sub(max_lines);
                Some(non_empty_lines[start..].join("\n"))
            }
            Self::OscTitle => osc_title.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_lines<'a>(screen_lines: &'a [String]) -> PaneSnapshot<'a> {
        PaneSnapshot {
            screen_lines,
            osc_title: None,
            foreground_process: None,
        }
    }

    fn lines(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    #[test]
    fn bottom_non_empty_lines_region_skips_blank_lines() {
        let manifest = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "working"
region = { bottom_non_empty_lines = 2 }
contains = "alpha"
"#,
        )
        .expect("parse");

        // "alpha" is third-from-last among non-blank lines, so a 2-line
        // region must not see it, no matter how many blank lines follow.
        let screen = lines(&["alpha", "", "beta", "", "", "gamma", "   "]);
        assert_eq!(manifest.detect(&snapshot_with_lines(&screen)), None);

        let screen = lines(&["alpha", "", "", "gamma", ""]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&screen)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn gates_and_together_and_any_is_or() {
        let manifest = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "blocked"
region = { bottom_non_empty_lines = 4 }
contains = "prompt"
any = ["Yes", "No"]
all = ["[", "]"]
not = "spinner"
"#,
        )
        .expect("parse");

        let matching = lines(&["prompt [Yes]"]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&matching)),
            Some(AgentState::Blocked)
        );

        // `any` alternative also passes.
        let matching_no = lines(&["prompt [No]"]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&matching_no)),
            Some(AgentState::Blocked)
        );

        // Missing the `contains` gate fails despite any/all passing.
        let missing_contains = lines(&["[Yes]"]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&missing_contains)),
            None
        );

        // Failing `all` fails.
        let missing_all = lines(&["prompt Yes"]);
        assert_eq!(manifest.detect(&snapshot_with_lines(&missing_all)), None);

        // `not` substring present fails.
        let with_not = lines(&["prompt [Yes] spinner"]);
        assert_eq!(manifest.detect(&snapshot_with_lines(&with_not)), None);
    }

    #[test]
    fn regex_gate_is_multiline_capable() {
        let manifest = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "idle"
region = { bottom_non_empty_lines = 6 }
regex = '(?m)^\s*>\s'
"#,
        )
        .expect("parse");

        let matching = lines(&["output", "  > waiting"]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&matching)),
            Some(AgentState::Idle)
        );

        let not_line_start = lines(&["a > b"]);
        assert_eq!(manifest.detect(&snapshot_with_lines(&not_line_start)), None);
    }

    #[test]
    fn highest_priority_wins_and_ties_keep_declaration_order() {
        let manifest = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
priority = 10
state = "idle"
region = { bottom_non_empty_lines = 4 }
contains = "shared"

[[rules]]
priority = 50
state = "working"
region = { bottom_non_empty_lines = 4 }
contains = "shared"

[[rules]]
priority = 50
state = "blocked"
region = { bottom_non_empty_lines = 4 }
contains = "shared"
"#,
        )
        .expect("parse");

        // Both priority-50 rules match; the first-declared (working) wins
        // over the later blocked rule, and both beat the priority-10 rule.
        let screen = lines(&["shared"]);
        assert_eq!(
            manifest.detect(&snapshot_with_lines(&screen)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn osc_title_region_matches_title_only() {
        let manifest = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "working"
region = "osc_title"
contains = "busy"
"#,
        )
        .expect("parse");

        let screen = lines(&["busy on screen but not in title"]);
        assert_eq!(manifest.detect(&snapshot_with_lines(&screen)), None);

        let snapshot = PaneSnapshot {
            screen_lines: &screen,
            osc_title: Some("agent busy"),
            foreground_process: None,
        };
        assert_eq!(manifest.detect(&snapshot), Some(AgentState::Working));
    }

    #[test]
    fn rule_without_gate_is_rejected() {
        let err = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "idle"
region = { bottom_non_empty_lines = 4 }
"#,
        )
        .expect_err("gateless rule must be rejected");
        assert!(err.contains("at least one gate"), "unexpected error: {err}");
    }

    #[test]
    fn invalid_state_and_region_are_rejected() {
        let bad_state = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "sleeping"
region = { bottom_non_empty_lines = 4 }
contains = "x"
"#,
        )
        .expect_err("unknown state must be rejected");
        assert!(bad_state.contains("unknown state"));

        let bad_region = AgentManifest::parse(
            r#"
name = "test"

[[rules]]
state = "idle"
region = "viewport"
contains = "x"
"#,
        )
        .expect_err("unknown region must be rejected");
        assert!(bad_region.contains("unknown region"));
    }
}
