//! External tool integrations.
//!
//! Currently: Claude Code. `spectra integration install claude` copies the
//! embedded hook script into the spectra data dir and registers it in
//! `~/.claude/settings.json` so Claude Code reports its semantic state
//! (`agent.report`) to the API socket of the spectra instance hosting it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::cli::{Cli, CliCommand, IntegrationAction};

/// Embedded hook script shipped inside the binary and materialized on
/// `integration install claude`.
pub const CLAUDE_HOOK_SCRIPT: &str = include_str!("assets/claude/spectra-agent-state.sh");

const CLAUDE_SCRIPT_FILE: &str = "spectra-agent-state.sh";
const CLAUDE_TOOL: &str = "claude";

/// Claude Code hook events we register for, with the optional matcher the
/// hooks schema requires for tool events.
const CLAUDE_HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("Stop", None),
    ("Notification", None),
    ("PreToolUse", Some("*")),
    ("UserPromptSubmit", None),
];

/// Entry point for `spectra integration ...` (CliMode::Integration).
pub fn run(cli: Cli) -> io::Result<()> {
    let Some(CliCommand::Integration { action }) = cli.subcommand else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing integration action",
        ));
    };
    let report = match action {
        IntegrationAction::Install { tool, dry_run } => {
            ensure_supported_tool(&tool)?;
            install_claude(&claude_script_path(), &claude_settings_path(), dry_run)?
        }
        IntegrationAction::Uninstall { tool } => {
            ensure_supported_tool(&tool)?;
            uninstall_claude(&claude_script_path(), &claude_settings_path())?
        }
    };
    println!("{report}");
    Ok(())
}

fn ensure_supported_tool(tool: &str) -> io::Result<()> {
    if tool == CLAUDE_TOOL {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown integration tool: {tool} (supported: claude)"),
    ))
}

/// Where the hook script is materialized (honors `$XDG_DATA_HOME`/`$HOME`).
pub fn claude_script_path() -> PathBuf {
    crate::xdg::app_data_dir()
        .join("integrations")
        .join("claude")
        .join(CLAUDE_SCRIPT_FILE)
}

/// Claude Code user settings file (honors `$HOME`).
pub fn claude_settings_path() -> PathBuf {
    crate::xdg::home_dir().join(".claude").join("settings.json")
}

/// Install (or refresh) the hook script and merge our hook entries into the
/// Claude Code settings file. Idempotent; `dry_run` only reports.
pub fn install_claude(
    script_path: &Path,
    settings_path: &Path,
    dry_run: bool,
) -> io::Result<String> {
    let existing = read_settings_text(settings_path)?;
    let mut settings = parse_settings(settings_path, existing.as_deref())?;
    let script = script_path.to_string_lossy().into_owned();
    let settings_changed = merge_claude_hooks(&mut settings, &script)?;
    let new_text = render_settings(&settings);

    if dry_run {
        let mut report = format!("dry-run: would install hook script: {}\n", script);
        if settings_changed {
            report.push_str(&format!(
                "dry-run: would update {} (backup kept as {}):\n{}",
                settings_path.display(),
                backup_path(settings_path).display(),
                settings_diff(existing.as_deref().unwrap_or(""), &new_text),
            ));
        } else {
            report.push_str(&format!(
                "dry-run: {} already contains the spectra hooks",
                settings_path.display()
            ));
        }
        return Ok(report);
    }

    write_hook_script(script_path)?;
    let mut report = format!("installed hook script: {script}");
    if settings_changed {
        write_backup_once(settings_path, existing.as_deref())?;
        write_atomic(settings_path, &new_text)?;
        report.push_str(&format!(
            "\nregistered Claude Code hooks in {} (Stop/Notification/PreToolUse/UserPromptSubmit)",
            settings_path.display()
        ));
    } else {
        report.push_str(&format!(
            "\n{} already contains the spectra hooks",
            settings_path.display()
        ));
    }
    Ok(report)
}

/// Remove our hook entries from the settings file and delete the script.
pub fn uninstall_claude(script_path: &Path, settings_path: &Path) -> io::Result<String> {
    let existing = read_settings_text(settings_path)?;
    let script = script_path.to_string_lossy().into_owned();
    let mut report = String::new();

    if existing.is_some() {
        let mut settings = parse_settings(settings_path, existing.as_deref())?;
        if remove_claude_hooks(&mut settings, &script)? {
            write_backup_once(settings_path, existing.as_deref())?;
            write_atomic(settings_path, &render_settings(&settings))?;
            report.push_str(&format!(
                "removed spectra hooks from {}\n",
                settings_path.display()
            ));
        } else {
            report.push_str(&format!(
                "no spectra hooks found in {}\n",
                settings_path.display()
            ));
        }
    } else {
        report.push_str(&format!("{} does not exist\n", settings_path.display()));
    }

    match fs::remove_file(script_path) {
        Ok(()) => report.push_str(&format!("removed hook script: {script}")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            report.push_str(&format!("hook script already absent: {script}"));
        }
        Err(err) => return Err(err),
    }
    Ok(report)
}

fn read_settings_text(settings_path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(settings_path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Parse-or-abort: a malformed settings file is never modified.
fn parse_settings(settings_path: &Path, existing: Option<&str>) -> io::Result<Value> {
    let Some(text) = existing else {
        return Ok(json!({}));
    };
    let value: Value = serde_json::from_str(text).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "refusing to modify {}: existing content is not valid JSON: {err}",
                settings_path.display()
            ),
        )
    })?;
    if !value.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "refusing to modify {}: top-level JSON value is not an object",
                settings_path.display()
            ),
        ));
    }
    Ok(value)
}

fn render_settings(settings: &Value) -> String {
    let mut text = serde_json::to_string_pretty(settings).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    text
}

fn hook_command(script_path: &str, event: &str) -> String {
    format!("\"{script_path}\" {event}")
}

fn is_our_hook(hook: &Value, script_path: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(script_path))
}

fn group_has_our_hook(group: &Value, script_path: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|hook| is_our_hook(hook, script_path)))
}

fn invalid_settings_shape(context: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("refusing to modify settings: {context}"),
    )
}

/// Merge our hook entries into `settings` (idempotent). Returns whether the
/// settings changed. The script path inside the command doubles as the tag
/// that makes our entries detectable on re-runs and uninstall.
fn merge_claude_hooks(settings: &mut Value, script_path: &str) -> io::Result<bool> {
    let root = settings
        .as_object_mut()
        .ok_or_else(|| invalid_settings_shape("top-level JSON value is not an object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| invalid_settings_shape("\"hooks\" is not an object"))?;

    let mut changed = false;
    for (event, matcher) in CLAUDE_HOOK_EVENTS {
        let entries = hooks.entry(*event).or_insert_with(|| json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| invalid_settings_shape(&format!("hooks.{event} is not an array")))?;
        if entries
            .iter()
            .any(|group| group_has_our_hook(group, script_path))
        {
            continue;
        }
        let mut group = Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".to_string(), json!(matcher));
        }
        group.insert(
            "hooks".to_string(),
            json!([{
                "type": "command",
                "command": hook_command(script_path, event),
                "timeout": 5,
            }]),
        );
        entries.push(Value::Object(group));
        changed = true;
    }
    Ok(changed)
}

/// Remove every hook entry whose command references `script_path`, pruning
/// groups/events/`hooks` only when our removal emptied them.
fn remove_claude_hooks(settings: &mut Value, script_path: &str) -> io::Result<bool> {
    let root = settings
        .as_object_mut()
        .ok_or_else(|| invalid_settings_shape("top-level JSON value is not an object"))?;
    let Some(hooks_value) = root.get_mut("hooks") else {
        return Ok(false);
    };
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| invalid_settings_shape("\"hooks\" is not an object"))?;

    let mut changed = false;
    let mut emptied_events = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(groups) = entries.as_array_mut() else {
            continue;
        };
        let mut removed_here = false;
        for group in groups.iter_mut() {
            let Some(group_hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = group_hooks.len();
            group_hooks.retain(|hook| !is_our_hook(hook, script_path));
            if group_hooks.len() != before {
                removed_here = true;
            }
        }
        if !removed_here {
            continue;
        }
        changed = true;
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
        if groups.is_empty() {
            emptied_events.push(event.clone());
        }
    }
    for event in emptied_events {
        hooks.remove(&event);
    }
    if changed && hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(changed)
}

fn write_hook_script(script_path: &Path) -> io::Result<()> {
    if let Some(parent) = script_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let current = fs::read_to_string(script_path).ok();
    if current.as_deref() != Some(CLAUDE_HOOK_SCRIPT) {
        fs::write(script_path, CLAUDE_HOOK_SCRIPT)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(script_path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn backup_path(settings_path: &Path) -> PathBuf {
    let mut name = settings_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    name.push_str(".bak");
    settings_path.with_file_name(name)
}

/// Keep a backup of the pristine pre-spectra settings: written only on the
/// first modification, never overwritten afterwards.
fn write_backup_once(settings_path: &Path, existing: Option<&str>) -> io::Result<()> {
    let Some(existing) = existing else {
        return Ok(());
    };
    let backup = backup_path(settings_path);
    if backup.exists() {
        return Ok(());
    }
    fs::write(&backup, existing)
}

/// Atomic replace: write a sibling temp file, then rename over the target.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    tmp_name.push_str(".spectra-tmp");
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, contents)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Minimal line diff for --dry-run: trims the common prefix/suffix and shows
/// the middle as removals/additions (our merge only inserts lines).
fn settings_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut out = String::new();
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;
