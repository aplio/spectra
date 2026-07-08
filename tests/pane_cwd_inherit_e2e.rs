//! End-to-end tests for a new pane/window inheriting the focused pane's
//! working directory. These drive a real `SessionManager` backed by real PTYs
//! and a real `/bin/sh`, exercising the full path: OSC 7 tracking on
//! `poll_output` and `focused_pane_cwd` feeding both the split and new-window
//! spawns.
//!
//! Each test starts the session with no configured cwd, so the session default
//! is the test process's directory. The only way the created pane can land in
//! `target` is by tracking the OSC 7 the focused pane emits beforehand — this
//! makes the assertion discriminate real inheritance from the session default.
#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pty_backend::PtyPaneFactory;
use spectra::ui::window_manager::{PaneId, SplitAxis};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Pump the session until the visible text of `pane_id` satisfies `predicate`
/// (or the timeout elapses), returning whatever text was last seen. Each turn
/// polls PTY output, which is what advances OSC 7 tracking and rendering.
fn poll_until(
    session: &mut SessionManager,
    pane_id: PaneId,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    loop {
        session.poll_output();
        let text = session
            .pane_screen_lines(pane_id)
            .map(|lines| lines.join("\n"))
            .unwrap_or_default();
        if predicate(&text) || start.elapsed() >= TIMEOUT {
            return text;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Screen text with all whitespace removed. Split panes are narrow enough
/// that a long path (e.g. a macOS `/private/var/folders/...` tempdir) hard-
/// wraps across rows; dropping whitespace rejoins the fragments so a
/// `contains` check on the path works regardless of pane width.
fn normalized(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The pane id created most recently (the highest id present).
fn newest_pane_id(session: &SessionManager) -> PaneId {
    session
        .all_pane_ids()
        .into_iter()
        .max()
        .expect("at least one pane")
}

/// Start a session (no configured cwd) and drive the focused pane to report
/// `target` via a real OSC 7 sequence, blocking until it has been processed.
/// Returns the session and the focused pane id.
fn session_with_focused_cwd(target_str: &str) -> (SessionManager, PaneId) {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(PtyPaneFactory), COLS, ROWS)
        .expect("create session");
    let focused = session.focused_pane_id().expect("focused pane");

    // Emit a real OSC 7 for `target`, then a marker whose token is split across
    // two quoted words so it appears only in command *output*, never in the
    // shell's echo of the typed line. Seeing it guarantees the OSC 7 that ran
    // just before it was already fed through `poll_output` (updating the pane's
    // tracked cwd) before we return.
    let cmd =
        format!("printf '\\033]7;file://localhost{target_str}\\007'; printf 'OSC''7_DONE\\n'\n");
    session
        .send_to_pane(focused, cmd.as_bytes())
        .expect("send osc7 emit");
    let marked = poll_until(&mut session, focused, |t| t.contains("OSC7_DONE"));
    assert!(
        marked.contains("OSC7_DONE"),
        "shell did not process the OSC 7 emit in time, saw:\n{marked}"
    );

    (session, focused)
}

/// Run `pwd` in `pane_id` and assert its output contains `target_str`. The
/// echoed `pwd` command does not contain the path, so a match proves the shell
/// was actually spawned in the inherited directory.
fn assert_pane_pwd_is(session: &mut SessionManager, pane_id: PaneId, target_str: &str) {
    session.send_to_pane(pane_id, b"pwd\n").expect("send pwd");
    let text = poll_until(session, pane_id, |t| normalized(t).contains(target_str));
    assert!(
        normalized(&text).contains(target_str),
        "pane {pane_id} should start in {target_str:?}, saw:\n{text}"
    );
}

#[test]
fn split_inherits_focused_pane_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = std::fs::canonicalize(dir.path()).expect("canonicalize target");
    let target_str = target.to_str().expect("utf8 target");

    let (mut session, focused) = session_with_focused_cwd(target_str);

    session
        .split_focused(SplitAxis::Vertical, COLS, ROWS)
        .expect("split");
    let new_pane = newest_pane_id(&session);
    assert_ne!(new_pane, focused, "split should create a distinct pane");

    assert_pane_pwd_is(&mut session, new_pane, target_str);
}

/// Full-path variant with a real bash: the OSC 7 comes from the shell
/// integration's prompt hook, not from a hand-written escape sequence. This
/// covers the emission half the other tests skip — it fails if the
/// integration rcfile stops being loaded (e.g. bash spawned as a login
/// shell, which ignores --rcfile) or if the hook stops reporting $PWD.
#[test]
fn split_inherits_cwd_reported_by_real_bash_prompt_hook() {
    let Some(bash) = ["/bin/bash", "/usr/bin/bash"]
        .iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        return; // no bash on this machine (e.g. minimal CI image)
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let target = std::fs::canonicalize(dir.path()).expect("canonicalize target");
    let target_str = target.to_str().expect("utf8 target");

    let options = SessionOptions::from_cli(Some(bash.to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(PtyPaneFactory), COLS, ROWS)
        .expect("create session");
    let focused = session.focused_pane_id().expect("focused pane");

    // cd and wait until the prompt hook's OSC 7 lands in the tracked cwd.
    // The session started in the test process's directory, so seeing
    // `target` here proves the hook emitted and the server parsed it.
    session
        .send_to_pane(focused, format!("cd {target_str}\n").as_bytes())
        .expect("send cd");
    let start = Instant::now();
    let tracked = loop {
        session.poll_output();
        let tracked = session.runtime_snapshot().pane_cwds.get(&focused).cloned();
        if tracked.as_deref() == Some(target.as_path()) || start.elapsed() >= TIMEOUT {
            break tracked;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        tracked.as_deref(),
        Some(target.as_path()),
        "bash prompt hook should report the cwd via OSC 7"
    );

    session
        .split_focused(SplitAxis::Vertical, COLS, ROWS)
        .expect("split");
    let new_pane = newest_pane_id(&session);
    assert_ne!(new_pane, focused, "split should create a distinct pane");

    assert_pane_pwd_is(&mut session, new_pane, target_str);
}

#[test]
fn new_window_inherits_focused_pane_cwd() {
    // The `prefix + c` path: a brand-new window's pane should also start in the
    // focused pane's directory.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = std::fs::canonicalize(dir.path()).expect("canonicalize target");
    let target_str = target.to_str().expect("utf8 target");

    let (mut session, focused) = session_with_focused_cwd(target_str);

    session.new_window(COLS, ROWS).expect("new window");
    let new_pane = newest_pane_id(&session);
    assert_ne!(
        new_pane, focused,
        "new window should create a distinct pane"
    );

    assert_pane_pwd_is(&mut session, new_pane, target_str);
}
