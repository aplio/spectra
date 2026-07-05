use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::{
    CLAUDE_HOOK_SCRIPT, install_claude, merge_claude_hooks, remove_claude_hooks, uninstall_claude,
};

struct Fixture {
    _dir: tempfile::TempDir,
    script: PathBuf,
    settings: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("create tempdir");
    let script = dir
        .path()
        .join("data")
        .join("integrations")
        .join("claude")
        .join("spectra-agent-state.sh");
    let settings = dir.path().join(".claude").join("settings.json");
    Fixture {
        _dir: dir,
        script,
        settings,
    }
}

fn read_settings(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read settings");
    serde_json::from_str(&text).expect("parse settings")
}

fn our_entries<'a>(settings: &'a Value, event: &str, script: &str) -> Vec<&'a Value> {
    settings["hooks"][event]
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .filter(|group| super::group_has_our_hook(group, script))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn embedded_asset_is_posix_sh_and_reports_agent_state() {
    assert!(CLAUDE_HOOK_SCRIPT.starts_with("#!/bin/sh"));
    assert!(CLAUDE_HOOK_SCRIPT.contains("agent.report"));
    assert!(CLAUDE_HOOK_SCRIPT.contains("SPECTRA_API_SOCKET"));
    assert!(CLAUDE_HOOK_SCRIPT.contains("SPECTRA_PANE_ID"));
    assert!(CLAUDE_HOOK_SCRIPT.contains("SPECTRA_SESSION_ID"));
}

#[test]
fn hook_script_exits_zero_outside_spectra() {
    let fx = fixture();
    fs::create_dir_all(fx.script.parent().expect("script parent")).expect("mkdir");
    fs::write(&fx.script, CLAUDE_HOOK_SCRIPT).expect("write script");

    let output = Command::new("sh")
        .arg(&fx.script)
        .arg("Stop")
        .env_remove("SPECTRA_API_SOCKET")
        .env_remove("SPECTRA_PANE_ID")
        .env_remove("SPECTRA_SESSION_ID")
        .stdin(Stdio::null())
        .output()
        .expect("run hook script");
    assert!(output.status.success(), "script should exit 0");
    assert!(output.stdout.is_empty(), "script should stay silent");
    assert!(output.stderr.is_empty(), "script should stay silent");
}

#[test]
fn install_creates_fresh_settings_with_all_events() {
    let fx = fixture();
    install_claude(&fx.script, &fx.settings, false).expect("install");

    assert_eq!(
        fs::read_to_string(&fx.script).expect("script written"),
        CLAUDE_HOOK_SCRIPT
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&fx.script)
            .expect("script metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "script must be executable");
    }

    let settings = read_settings(&fx.settings);
    let script = fx.script.to_string_lossy().into_owned();
    for event in ["Stop", "Notification", "PreToolUse", "UserPromptSubmit"] {
        let ours = our_entries(&settings, event, &script);
        assert_eq!(ours.len(), 1, "expected one spectra entry for {event}");
        let command = ours[0]["hooks"][0]["command"]
            .as_str()
            .expect("command string");
        assert!(command.ends_with(&format!(" {event}")));
    }
    assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], json!("*"));
    assert!(settings["hooks"]["Stop"][0].get("matcher").is_none());
}

#[test]
fn install_preserves_existing_unrelated_settings_and_hooks() {
    let fx = fixture();
    let original = json!({
        "model": "opus",
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "echo done"}]}],
            "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo post"}]}]
        }
    });
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    let original_text = serde_json::to_string_pretty(&original).expect("serialize");
    fs::write(&fx.settings, &original_text).expect("seed settings");

    install_claude(&fx.script, &fx.settings, false).expect("install");

    let settings = read_settings(&fx.settings);
    assert_eq!(settings["model"], json!("opus"));
    let stop = settings["hooks"]["Stop"].as_array().expect("Stop array");
    assert_eq!(stop.len(), 2, "existing Stop hook preserved plus ours");
    assert_eq!(stop[0]["hooks"][0]["command"], json!("echo done"));
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        json!("echo post")
    );

    let backup = super::backup_path(&fx.settings);
    assert_eq!(
        fs::read_to_string(&backup).expect("backup written"),
        original_text,
        "backup keeps the pre-modification content"
    );
}

#[test]
fn install_is_idempotent() {
    let fx = fixture();
    install_claude(&fx.script, &fx.settings, false).expect("first install");
    let first = fs::read_to_string(&fx.settings).expect("read first");
    let report = install_claude(&fx.script, &fx.settings, false).expect("second install");
    let second = fs::read_to_string(&fx.settings).expect("read second");
    assert_eq!(first, second, "re-run must not duplicate entries");
    assert!(report.contains("already contains"));

    let settings = read_settings(&fx.settings);
    let script = fx.script.to_string_lossy().into_owned();
    for event in ["Stop", "Notification", "PreToolUse", "UserPromptSubmit"] {
        assert_eq!(our_entries(&settings, event, &script).len(), 1);
    }
}

#[test]
fn uninstall_removes_only_our_entries_and_the_script() {
    let fx = fixture();
    let original = json!({
        "model": "opus",
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "echo done"}]}]
        }
    });
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    fs::write(
        &fx.settings,
        serde_json::to_string_pretty(&original).expect("serialize"),
    )
    .expect("seed settings");

    install_claude(&fx.script, &fx.settings, false).expect("install");
    uninstall_claude(&fx.script, &fx.settings).expect("uninstall");

    let settings = read_settings(&fx.settings);
    assert_eq!(settings["model"], json!("opus"));
    let stop = settings["hooks"]["Stop"].as_array().expect("Stop array");
    assert_eq!(stop.len(), 1, "only the unrelated Stop hook remains");
    assert_eq!(stop[0]["hooks"][0]["command"], json!("echo done"));
    for event in ["Notification", "PreToolUse", "UserPromptSubmit"] {
        assert!(
            settings["hooks"].get(event).is_none(),
            "{event} emptied by our removal must be pruned"
        );
    }
    assert!(!fx.script.exists(), "script must be deleted");
}

#[test]
fn uninstall_on_pristine_settings_is_a_noop() {
    let fx = fixture();
    let original_text = "{\n  \"model\": \"opus\"\n}\n";
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    fs::write(&fx.settings, original_text).expect("seed settings");

    let report = uninstall_claude(&fx.script, &fx.settings).expect("uninstall");
    assert!(report.contains("no spectra hooks found"));
    assert_eq!(
        fs::read_to_string(&fx.settings).expect("read settings"),
        original_text,
        "file untouched when nothing to remove"
    );
    assert!(!super::backup_path(&fx.settings).exists());
}

#[test]
fn malformed_settings_error_leaves_file_and_backup_untouched() {
    let fx = fixture();
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    fs::write(&fx.settings, "{ not json").expect("seed malformed");

    let err = install_claude(&fx.script, &fx.settings, false).expect_err("must refuse");
    assert!(err.to_string().contains("not valid JSON"));
    assert_eq!(
        fs::read_to_string(&fx.settings).expect("read settings"),
        "{ not json",
        "malformed settings must never be clobbered"
    );
    assert!(!super::backup_path(&fx.settings).exists());

    let err = uninstall_claude(&fx.script, &fx.settings).expect_err("uninstall must refuse too");
    assert!(err.to_string().contains("not valid JSON"));
}

#[test]
fn non_object_settings_are_rejected() {
    let fx = fixture();
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    fs::write(&fx.settings, "[1, 2]").expect("seed array settings");

    let err = install_claude(&fx.script, &fx.settings, false).expect_err("must refuse");
    assert!(err.to_string().contains("not an object"));
    assert_eq!(
        fs::read_to_string(&fx.settings).expect("read settings"),
        "[1, 2]"
    );
}

#[test]
fn dry_run_reports_without_writing() {
    let fx = fixture();
    let report = install_claude(&fx.script, &fx.settings, true).expect("dry run");
    assert!(report.contains(&fx.script.to_string_lossy().into_owned()));
    assert!(report.contains("agent-state"));
    assert!(report.contains("+ "), "diff must show additions");
    assert!(!fx.script.exists(), "dry-run must not write the script");
    assert!(!fx.settings.exists(), "dry-run must not write settings");
}

#[test]
fn atomic_write_leaves_no_temp_file() {
    let fx = fixture();
    install_claude(&fx.script, &fx.settings, false).expect("install");
    let parent = fx.settings.parent().expect("settings parent");
    let leftovers: Vec<_> = fs::read_dir(parent)
        .expect("read settings dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn backup_keeps_first_version_across_later_modifications() {
    let fx = fixture();
    let original_text = "{\n  \"model\": \"opus\"\n}\n";
    fs::create_dir_all(fx.settings.parent().expect("settings parent")).expect("mkdir");
    fs::write(&fx.settings, original_text).expect("seed settings");

    install_claude(&fx.script, &fx.settings, false).expect("install");
    uninstall_claude(&fx.script, &fx.settings).expect("uninstall");

    assert_eq!(
        fs::read_to_string(super::backup_path(&fx.settings)).expect("read backup"),
        original_text,
        ".bak must keep the pristine first version"
    );
}

#[test]
fn merge_and_remove_round_trip_restores_settings() {
    let mut settings = json!({
        "permissions": {"allow": ["Bash(ls:*)"]},
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "echo done"}]}]
        }
    });
    let reference = settings.clone();
    assert!(merge_claude_hooks(&mut settings, "/data/spectra-agent-state.sh").expect("merge"));
    assert!(
        !merge_claude_hooks(&mut settings, "/data/spectra-agent-state.sh").expect("re-merge"),
        "second merge reports no change"
    );
    assert!(remove_claude_hooks(&mut settings, "/data/spectra-agent-state.sh").expect("remove"));
    assert_eq!(settings, reference, "remove must restore the original");
    assert!(
        !remove_claude_hooks(&mut settings, "/data/spectra-agent-state.sh").expect("re-remove"),
        "second remove reports no change"
    );
}
