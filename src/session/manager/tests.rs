use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use crate::session::pane::{FakeBackend, PaneBackend};
use crate::session::pty_backend::{PaneFactory, PaneSpawnConfig};
use crate::session::terminal_state::{StyledCell, TerminalEvent};
use crate::ui::window_manager::{Direction, DividerOrientation, SplitAxis};

use super::{PaneTerminalEvent, SessionManager, SessionOptions, SessionRuntimeSnapshot};

struct FakeFactory;

impl PaneFactory for FakeFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(FakeBackend::new(vec![b"pane".to_vec()])))
    }
}

struct ClosedBackend;

impl PaneBackend for ClosedBackend {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn is_closed(&mut self) -> bool {
        true
    }
}

struct ClosedFactory;

impl PaneFactory for ClosedFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(ClosedBackend))
    }
}

#[derive(Clone)]
struct RecordingFactory {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

struct RecordingBackend {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PaneBackend for RecordingBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes
            .lock()
            .expect("recording writes lock")
            .push(bytes.to_vec());
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

impl PaneFactory for RecordingFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(RecordingBackend {
            writes: Arc::clone(&self.writes),
        }))
    }
}

#[derive(Clone)]
struct SpawnConfigFactory {
    configs: Arc<Mutex<Vec<PaneSpawnConfig>>>,
}

impl PaneFactory for SpawnConfigFactory {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        self.configs
            .lock()
            .expect("spawn config lock")
            .push(config.clone());
        Ok(Box::new(FakeBackend::new(vec![])))
    }
}

struct StaticOutputFactory {
    bytes: Vec<u8>,
}

impl PaneFactory for StaticOutputFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(FakeBackend::new(vec![self.bytes.clone()])))
    }
}

#[derive(Clone)]
struct IncrementalOutputFactory {
    chunks: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

struct IncrementalOutputBackend {
    chunks: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl PaneBackend for IncrementalOutputBackend {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = self.chunks.lock().expect("incremental output lock");
        chunks
            .pop_front()
            .map_or_else(Vec::new, |chunk| vec![chunk])
    }
}

impl PaneFactory for IncrementalOutputFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(IncrementalOutputBackend {
            chunks: Arc::clone(&self.chunks),
        }))
    }
}

fn trimmed_row_text(cells: &[StyledCell]) -> String {
    cells
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn split_creates_new_pane() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");

    assert_eq!(session.pane_count(), 1);
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");
    assert_eq!(session.pane_count(), 2);
    assert_eq!(session.window_count(), 1);
}

/// Factory that records every spawn cwd and makes only its first spawned
/// backend emit an OSC 7 sequence, so a split can be asserted to inherit the
/// focused pane's tracked cwd (either seeded at spawn or reported via OSC 7).
struct Osc7SplitFactory {
    configs: Arc<Mutex<Vec<PaneSpawnConfig>>>,
    first_output: Mutex<Option<Vec<u8>>>,
}

impl PaneFactory for Osc7SplitFactory {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        self.configs
            .lock()
            .expect("spawn config lock")
            .push(config.clone());
        let output = self
            .first_output
            .lock()
            .expect("first output lock")
            .take()
            .map(|bytes| vec![bytes])
            .unwrap_or_default();
        Ok(Box::new(FakeBackend::new(output)))
    }
}

#[test]
fn split_inherits_seeded_spawn_cwd() {
    // The session cwd is seeded onto the initial pane at spawn; a split before
    // any OSC 7 is emitted must still inherit that directory.
    let dir = std::env::temp_dir();
    let configs = Arc::new(Mutex::new(Vec::new()));
    let factory = SpawnConfigFactory {
        configs: Arc::clone(&configs),
    };
    let mut options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    options.cwd = Some(dir.clone());
    let mut session =
        SessionManager::with_factory(options, Arc::new(factory), 80, 24).expect("create session");

    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");

    let configs = configs.lock().expect("configs lock");
    assert_eq!(
        configs.len(),
        2,
        "one spawn for the initial pane, one for split"
    );
    assert_eq!(configs[0].cwd.as_deref(), Some(dir.as_path()));
    assert_eq!(configs[1].cwd.as_deref(), Some(dir.as_path()));
}

#[test]
fn split_inherits_osc7_reported_cwd() {
    // Session cwd is unset, so inheritance can only come from the OSC 7 the
    // focused pane emits before the split.
    let dir = std::env::temp_dir();
    let dir_str = dir.to_str().expect("utf8 temp dir");
    let osc7 = format!("\x1b]7;file://localhost{dir_str}\x07").into_bytes();

    let configs = Arc::new(Mutex::new(Vec::new()));
    let factory = Osc7SplitFactory {
        configs: Arc::clone(&configs),
        first_output: Mutex::new(Some(osc7)),
    };
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(factory), 80, 24).expect("create session");

    // Drive the focused pane so it consumes the OSC 7 sequence.
    session.poll_output();

    // The event still flows to the App layer (pane naming keeps working).
    assert!(
        session.take_terminal_events().iter().any(
            |event| matches!(&event.event, TerminalEvent::CwdChanged { cwd } if cwd == dir_str)
        ),
        "CwdChanged event should still be surfaced to the App",
    );

    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");

    let configs = configs.lock().expect("configs lock");
    assert_eq!(configs.len(), 2);
    assert_eq!(
        configs[0].cwd, None,
        "initial pane spawns with the session cwd"
    );
    assert_eq!(configs[1].cwd.as_deref(), Some(dir.as_path()));
}

#[test]
fn set_host_colors_updates_existing_and_future_panes() {
    use crate::io::host_colors::HostColors;

    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    assert_eq!(session.host_colors(), HostColors::default());

    let colors = HostColors {
        fg: Some((0xff, 0xff, 0xff)),
        bg: Some((0x1e, 0x2a, 0x3c)),
    };
    session.set_host_colors(colors);
    assert_eq!(session.host_colors(), colors);
    for pane in session.panes.values() {
        assert_eq!(pane.host_colors(), colors);
    }

    // Panes spawned after the report inherit the cached colors too.
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");
    assert_eq!(session.pane_count(), 2);
    for pane in session.panes.values() {
        assert_eq!(pane.host_colors(), colors);
    }
}

#[test]
fn new_window_creates_new_pane() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");

    session.new_window(80, 24).expect("create new window");
    assert_eq!(session.pane_count(), 2);
    assert_eq!(session.window_count(), 2);
}

#[test]
fn tmux_passthrough_output_respects_allow_passthrough_toggle() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let wrapped = b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\".to_vec();

    let mut enabled = SessionManager::with_factory(
        options.clone(),
        Arc::new(StaticOutputFactory {
            bytes: wrapped.clone(),
        }),
        80,
        24,
    )
    .expect("create passthrough-enabled session");
    assert!(enabled.allow_passthrough());
    assert!(enabled.poll_output(), "expected pane output");
    assert_eq!(
        enabled.take_passthrough_output(),
        vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]
    );

    let mut disabled = SessionManager::with_factory(
        options,
        Arc::new(StaticOutputFactory { bytes: wrapped }),
        80,
        24,
    )
    .expect("create passthrough-disabled session");
    disabled.set_allow_passthrough(false);
    assert!(!disabled.allow_passthrough());
    assert!(disabled.poll_output(), "expected pane output");
    assert!(disabled.take_passthrough_output().is_empty());
}

#[test]
fn osc8_is_not_forwarded_to_host_passthrough() {
    // Regression: OSC 8 hyperlinks are modelled per-cell and re-emitted by the
    // renderer aligned with the frame. Forwarding the raw guest sequence to the
    // host as well used to leave the host stuck in an open hyperlink state (its
    // open and close arriving in different output bursts), bleeding underline
    // across the whole frame including spectra's own status line. OSC 8 must not
    // reach the host passthrough channel, even when passthrough is enabled.
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let bytes = b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07".to_vec();

    let mut session =
        SessionManager::with_factory(options, Arc::new(StaticOutputFactory { bytes }), 80, 24)
            .expect("create passthrough-enabled session");
    assert!(session.allow_passthrough());
    assert!(session.poll_output(), "expected pane output");
    assert!(
        session.take_passthrough_output().is_empty(),
        "OSC 8 must not be forwarded to the host"
    );
}

#[test]
fn poll_output_collects_terminal_events_with_pane_id() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(
        options,
        Arc::new(StaticOutputFactory {
            bytes: b"\x1b]0;build\x07".to_vec(),
        }),
        80,
        24,
    )
    .expect("create session");

    assert!(session.poll_output(), "expected pane output");
    assert_eq!(
        session.take_terminal_events(),
        vec![PaneTerminalEvent {
            pane_id: 1,
            event: TerminalEvent::TitleChanged {
                title: Some("build".to_string())
            }
        }]
    );
}

#[test]
fn new_window_with_command_uses_override_without_mutating_default() {
    let configs = Arc::new(Mutex::new(Vec::new()));
    let options = SessionOptions::from_cli(
        Some("/bin/sh".to_string()),
        None,
        vec!["echo base".to_string()],
    );
    let mut session = SessionManager::with_factory(
        options,
        Arc::new(SpawnConfigFactory {
            configs: Arc::clone(&configs),
        }),
        80,
        24,
    )
    .expect("create session");

    session
        .new_window_with_command(80, 24, vec!["echo editor".to_string()])
        .expect("create editor window");
    session.new_window(80, 24).expect("create default window");

    let recorded = configs.lock().expect("spawn config lock");
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].command, vec!["echo base".to_string()]);
    assert_eq!(recorded[1].command, vec!["echo editor".to_string()]);
    assert_eq!(recorded[2].command, vec!["echo base".to_string()]);
}

#[test]
fn spawned_panes_receive_assigned_pane_and_session_ids() {
    let configs = Arc::new(Mutex::new(Vec::new()));
    let mut options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    options.session_id = "main-1".to_string();
    let mut session = SessionManager::with_factory(
        options,
        Arc::new(SpawnConfigFactory {
            configs: Arc::clone(&configs),
        }),
        80,
        24,
    )
    .expect("create session");

    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    session.new_window(80, 24).expect("create window");

    let recorded = configs.lock().expect("spawn config lock");
    let ids: Vec<usize> = recorded.iter().map(|config| config.pane_id).collect();
    assert_eq!(ids, vec![1, 2, 3], "pane ids must follow assignment order");
    assert!(
        recorded.iter().all(|config| config.session_id == "main-1"),
        "every spawn must carry the session id"
    );
}

#[test]
fn frame_follows_cursor_when_viewport_is_shorter_than_buffer() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut output = String::new();
    for index in 0..64 {
        output.push_str(&format!("line-{index:03}\r\n"));
    }
    output.push_str("line-END");

    let mut session = SessionManager::with_factory(
        options,
        Arc::new(StaticOutputFactory {
            bytes: output.into_bytes(),
        }),
        80,
        24,
    )
    .expect("create session");
    assert!(session.poll_output(), "expected pane output");

    let frame = session.frame(80, 8);
    let pane = frame.panes.first().expect("pane frame");
    let visible = pane
        .rows
        .iter()
        .map(|row| trimmed_row_text(row))
        .collect::<Vec<_>>();
    assert!(
        visible.iter().any(|line| line.contains("line-END")),
        "expected viewport to follow tail output, rows={visible:?}"
    );

    let (_, cursor_y) = frame.focused_cursor.expect("focused cursor");
    let pane_bottom = (pane.rect.y + pane.rect.height.saturating_sub(1)) as u16;
    assert!(
        cursor_y >= pane_bottom.saturating_sub(1),
        "expected cursor near pane bottom, got y={cursor_y}, bottom={pane_bottom}"
    );
}

#[test]
fn scrolled_view_stays_pinned_when_new_output_arrives() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut initial = String::new();
    for index in 0..80 {
        initial.push_str(&format!("line-{index:03}\r\n"));
    }
    let chunks = Arc::new(Mutex::new(VecDeque::from(vec![
        initial.into_bytes(),
        b"line-APPEND\r\n".to_vec(),
    ])));

    let mut session = SessionManager::with_factory(
        options,
        Arc::new(IncrementalOutputFactory { chunks }),
        80,
        24,
    )
    .expect("create session");
    assert!(session.poll_output(), "expected initial pane output");

    let pane_view_rows = session
        .frame(80, 24)
        .panes
        .first()
        .expect("pane frame")
        .rect
        .height;
    session.scroll_focused_pane(5, pane_view_rows);

    let origin_before = session
        .focused_view_row_origin(pane_view_rows)
        .expect("origin before");
    let top_line_before = {
        let frame = session.frame(80, 24);
        let pane = frame.panes.first().expect("pane frame before");
        trimmed_row_text(&pane.rows[0])
    };

    assert!(session.poll_output(), "expected append output");

    let origin_after = session
        .focused_view_row_origin(pane_view_rows)
        .expect("origin after");
    let top_line_after = {
        let frame = session.frame(80, 24);
        let pane = frame.panes.first().expect("pane frame after");
        trimmed_row_text(&pane.rows[0])
    };

    assert_eq!(origin_after, origin_before);
    assert_eq!(top_line_after, top_line_before);
}

#[test]
fn clear_screen_redraw_does_not_show_stale_scrollback_in_follow_mode() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut output = String::new();
    for index in 0..64 {
        output.push_str(&format!("line-{index:03}\r\n"));
    }
    output.push_str("\x1b[H\x1b[2Jprompt$ ");

    let mut session = SessionManager::with_factory(
        options,
        Arc::new(StaticOutputFactory {
            bytes: output.into_bytes(),
        }),
        80,
        24,
    )
    .expect("create session");
    assert!(session.poll_output(), "expected pane output");

    let frame = session.frame(80, 24);
    let pane = frame.panes.first().expect("pane frame");
    let visible = pane
        .rows
        .iter()
        .map(|row| trimmed_row_text(row))
        .collect::<Vec<_>>();

    assert_eq!(visible[0], "prompt$");
    assert!(
        !visible.iter().any(|line| line.starts_with("line-")),
        "expected cleared screen without stale rows, rows={visible:?}"
    );
}

#[test]
fn frame_origin_stays_zero_when_viewport_not_smaller() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let output = b"line-000\r\nline-001\r\nline-002".to_vec();
    let mut session = SessionManager::with_factory(
        options,
        Arc::new(StaticOutputFactory { bytes: output }),
        80,
        24,
    )
    .expect("create session");
    assert!(session.poll_output(), "expected pane output");

    let frame = session.frame(80, 30);
    let pane = frame.panes.first().expect("pane frame");
    assert_eq!(trimmed_row_text(&pane.rows[0]), "line-000");
    assert_eq!(trimmed_row_text(&pane.rows[1]), "line-001");
    assert_eq!(trimmed_row_text(&pane.rows[2]), "line-002");
}

#[test]
fn peek_all_panes_frame_tiles_panes_from_all_windows() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split first window");
    session.new_window(80, 24).expect("new window");
    session
        .split_focused(SplitAxis::Horizontal, 80, 24)
        .expect("split second window");

    let frame = session.peek_all_panes_frame(80, 24);
    assert_eq!(frame.panes.len(), 4);

    let pane_ids = frame
        .panes
        .iter()
        .map(|pane| pane.pane_id)
        .collect::<Vec<_>>();
    assert_eq!(pane_ids, vec![1, 2, 3, 4]);

    assert!(
        frame
            .panes
            .iter()
            .any(|pane| pane.focused && pane.pane_id == 4)
    );
    assert!(
        frame
            .dividers
            .iter()
            .any(|divider| divider.orientation == DividerOrientation::Vertical)
    );
    assert!(
        frame
            .dividers
            .iter()
            .any(|divider| divider.orientation == DividerOrientation::Horizontal)
    );
}

#[test]
fn peek_all_panes_frame_includes_hidden_panes_when_window_zoomed() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split window");

    session
        .toggle_zoom_active_window(80, 24)
        .expect("zoom active window");
    let zoomed = session.frame(80, 24);
    assert_eq!(zoomed.panes.len(), 1, "zoomed frame should show one pane");

    let peek = session.peek_all_panes_frame(80, 24);
    assert_eq!(peek.panes.len(), 2, "peek frame should show all panes");
}

#[test]
fn resize_preserves_existing_pane_content() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    assert!(session.poll_output(), "expected initial pane output");

    let before = session.frame(80, 24);
    let before_row = before.panes[0].rows[0]
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>();
    assert!(
        before_row.starts_with("pane"),
        "expected pane text before resize, got {before_row:?}"
    );

    session.resize(100, 30).expect("resize session");
    let after = session.frame(100, 30);
    let after_row = after.panes[0].rows[0]
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>();
    assert!(
        after_row.starts_with("pane"),
        "expected pane text to survive resize, got {after_row:?}"
    );
}

#[test]
fn window_navigation_and_swap_work() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session.new_window(80, 24).expect("new window 2");
    session.new_window(80, 24).expect("new window 3");

    assert_eq!(session.focused_window_number(), Some(3));
    session.focus_prev_window().expect("focus prev window");
    assert_eq!(session.focused_window_number(), Some(2));
    session.focus_next_window().expect("focus next window");
    assert_eq!(session.focused_window_number(), Some(3));

    let before = session
        .window_entries()
        .into_iter()
        .map(|e| e.window_id)
        .collect::<Vec<_>>();
    session.swap_prev_window().expect("swap prev window");
    let after = session
        .window_entries()
        .into_iter()
        .map(|e| e.window_id)
        .collect::<Vec<_>>();

    assert_ne!(before, after);
}

#[test]
fn focus_pane_id_switches_focus() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");
    let pane_ids = session.all_pane_ids();
    assert_eq!(pane_ids.len(), 2);

    session
        .focus_pane_id(pane_ids[0])
        .expect("focus first pane by id");
    assert_eq!(session.focused_pane_id(), Some(pane_ids[0]));
    assert!(session.focus_pane_id(999).is_err());
}

#[test]
fn resize_focused_changes_layout() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");
    session
        .focus(Direction::Left, 80, 24)
        .expect("focus left pane");

    let before = session.layout_snapshot(80, 24);
    let left_before = before.windows[0]
        .panes
        .iter()
        .find(|pane| pane.pane_id == 1)
        .expect("left pane")
        .rect
        .width;

    session
        .resize_focused(Direction::Left, 10, 80, 24)
        .expect("resize focused pane");

    let after = session.layout_snapshot(80, 24);
    let left_after = after.windows[0]
        .panes
        .iter()
        .find(|pane| pane.pane_id == 1)
        .expect("left pane")
        .rect
        .width;

    assert!(left_after > left_before);
}

#[test]
fn close_pane_can_close_non_focused_pane() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split vertical");

    assert_eq!(session.focused_pane_id(), Some(2));
    assert!(session.pane_exists(1));
    assert!(session.pane_exists(2));

    session.close_pane(1, 80, 24).expect("close pane by id");

    assert_eq!(session.pane_count(), 1);
    assert_eq!(session.focused_pane_id(), Some(2));
    assert!(!session.pane_exists(1));
}

#[test]
fn rename_session_updates_name() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");

    session.rename_session("dev".to_string());

    assert_eq!(session.session_name(), "dev");
}

#[test]
fn runtime_snapshot_roundtrip_restores_topology_and_focus() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options.clone(), Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split first window");
    session.new_window(80, 24).expect("create second window");
    session.focus_window_number(1).expect("focus first window");

    let first_window_panes = session
        .pane_ids_for_window_number(1)
        .expect("first window panes");
    let focused_pane = first_window_panes[1];
    session
        .focus_pane_id(focused_pane)
        .expect("focus pane in first window");

    let second_window_panes = session
        .pane_ids_for_window_number(2)
        .expect("second window panes");
    assert_eq!(second_window_panes.len(), 1);

    let snapshot = session.runtime_snapshot();
    let mut restored = SessionManager::with_factory_from_runtime_snapshot(
        options,
        Arc::new(FakeFactory),
        snapshot,
        80,
        24,
    )
    .expect("restore session from runtime snapshot");

    assert_eq!(restored.window_count(), 2);
    assert_eq!(restored.pane_count(), 3);
    assert_eq!(restored.focused_window_number(), Some(1));
    assert_eq!(restored.focused_pane_id(), Some(focused_pane));
    assert_eq!(
        restored
            .pane_ids_for_window_number(1)
            .expect("restored first window panes")
            .len(),
        2
    );
    assert_eq!(
        restored
            .pane_ids_for_window_number(2)
            .expect("restored second window panes"),
        second_window_panes
    );

    let before_max_pane = restored
        .all_pane_ids()
        .into_iter()
        .max()
        .expect("existing panes");
    restored
        .new_window(80, 24)
        .expect("new window after restore");
    let after_max_pane = restored
        .all_pane_ids()
        .into_iter()
        .max()
        .expect("panes after new window");
    assert_eq!(after_max_pane, before_max_pane + 1);
}

#[test]
fn zoom_toggle_roundtrip_restores_layout() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split first");
    session
        .split_focused(SplitAxis::Horizontal, 80, 24)
        .expect("split second");

    let before = session.layout_snapshot(80, 24);
    let before_panes = before.windows[0]
        .panes
        .iter()
        .map(|pane| (pane.pane_id, pane.rect))
        .collect::<Vec<_>>();
    assert_eq!(before_panes.len(), 3);

    let zoomed = session
        .toggle_zoom_active_window(80, 24)
        .expect("toggle zoom on");
    assert!(zoomed);
    assert!(session.active_window_zoomed());
    assert_eq!(session.frame(80, 24).panes.len(), 1);

    let zoomed = session
        .toggle_zoom_active_window(80, 24)
        .expect("toggle zoom off");
    assert!(!zoomed);
    assert!(!session.active_window_zoomed());

    let after = session.layout_snapshot(80, 24);
    let after_panes = after.windows[0]
        .panes
        .iter()
        .map(|pane| (pane.pane_id, pane.rect))
        .collect::<Vec<_>>();
    assert_eq!(after_panes, before_panes);
}

#[test]
fn synchronize_panes_fans_out_bytes_to_all_panes_in_window() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = RecordingFactory {
        writes: Arc::clone(&writes),
    };
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(factory), 80, 24).expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split window");

    assert!(!session.active_window_synchronize_panes());
    let sync = session
        .toggle_synchronize_panes_active_window()
        .expect("toggle sync on");
    assert!(sync);

    let sent = session
        .send_to_active_window(b"hello")
        .expect("send to active window");
    assert_eq!(sent, 2);
    let recorded = std::mem::take(&mut *writes.lock().expect("recorded writes lock"));
    assert_eq!(recorded, vec![b"hello".to_vec(), b"hello".to_vec()]);
}

#[test]
fn runtime_snapshot_preserves_zoom_and_synchronize_flags() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options.clone(), Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split window");
    session
        .toggle_synchronize_panes_active_window()
        .expect("enable sync");
    session
        .toggle_zoom_active_window(80, 24)
        .expect("enable zoom");

    let snapshot = session.runtime_snapshot();
    let restored = SessionManager::with_factory_from_runtime_snapshot(
        options,
        Arc::new(FakeFactory),
        snapshot,
        80,
        24,
    )
    .expect("restore session");

    assert!(restored.active_window_zoomed());
    assert!(restored.active_window_synchronize_panes());
    assert_eq!(restored.pane_count(), 2);
}

#[test]
fn restore_rejects_snapshot_without_windows() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let snapshot = SessionRuntimeSnapshot {
        session_name: "main".to_string(),
        next_pane_id: 1,
        next_window_id: 1,
        active_window: 0,
        windows: Vec::new(),
    };

    let err = match SessionManager::with_factory_from_runtime_snapshot(
        options,
        Arc::new(FakeFactory),
        snapshot,
        80,
        24,
    ) {
        Ok(_) => panic!("invalid snapshot should fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("no windows"));
}

#[test]
fn focused_pane_closed_detects_terminated_backend() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options, Arc::new(ClosedFactory), 80, 24)
        .expect("create session");

    assert!(session.focused_pane_closed());
}

#[test]
fn restore_accepts_legacy_snapshot_missing_optional_window_fields() {
    // Snapshots written before zoom/sync persistence lack `zoomed`,
    // `synchronize_panes` and `zoom_snapshot`; restoring one must fall back
    // to serde defaults instead of failing.
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options.clone(), Arc::new(FakeFactory), 80, 24)
        .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split window");

    let mut value = serde_json::to_value(session.runtime_snapshot()).expect("serialize snapshot");
    for window in value["windows"]
        .as_array_mut()
        .expect("windows array")
        .iter_mut()
    {
        let map = window.as_object_mut().expect("window object");
        map.remove("zoomed");
        map.remove("synchronize_panes");
        map.remove("zoom_snapshot");
    }
    let legacy: SessionRuntimeSnapshot =
        serde_json::from_value(value).expect("legacy snapshot must deserialize");

    let restored = SessionManager::with_factory_from_runtime_snapshot(
        options,
        Arc::new(FakeFactory),
        legacy,
        80,
        24,
    )
    .expect("restore legacy snapshot");
    assert_eq!(restored.pane_count(), 2);
    assert!(!restored.active_window_zoomed());
    assert!(!restored.active_window_synchronize_panes());
}

#[test]
fn restore_clamps_out_of_range_active_window_index() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(options.clone(), Arc::new(FakeFactory), 80, 24)
        .expect("create session");

    let mut snapshot = session.runtime_snapshot();
    snapshot.active_window = 9;

    let restored = SessionManager::with_factory_from_runtime_snapshot(
        options,
        Arc::new(FakeFactory),
        snapshot,
        80,
        24,
    )
    .expect("restore snapshot with bogus active window");
    assert_eq!(restored.focused_window_number(), Some(1));
}

#[test]
fn send_paste_to_active_window_wraps_only_bracketed_panes() {
    type PasteWrites = std::sync::Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>;
    struct PasteRecordingBackend {
        pane_id: usize,
        writes: PasteWrites,
    }
    impl PaneBackend for PasteRecordingBackend {
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes
                .lock()
                .expect("paste writes lock")
                .push((self.pane_id, bytes.to_vec()));
            Ok(())
        }
        fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
            Ok(())
        }
        fn poll_output(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }
    }
    struct PasteRecordingFactory {
        writes: PasteWrites,
    }
    impl PaneFactory for PasteRecordingFactory {
        fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
            Ok(Box::new(PasteRecordingBackend {
                pane_id: config.pane_id,
                writes: std::sync::Arc::clone(&self.writes),
            }))
        }
    }

    let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(
        options,
        Arc::new(PasteRecordingFactory {
            writes: std::sync::Arc::clone(&writes),
        }),
        80,
        24,
    )
    .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, 80, 24)
        .expect("split window");
    assert!(session.feed_pane_replay(2, b"\x1b[?2004h"));

    let sent = session
        .send_paste_to_active_window("txt")
        .expect("paste to window");

    assert_eq!(sent, 2);
    let mut by_pane = writes.lock().expect("paste writes lock").clone();
    by_pane.sort_by_key(|(pane, _)| *pane);
    assert_eq!(
        by_pane,
        vec![(1, b"txt".to_vec()), (2, b"\x1b[200~txt\x1b[201~".to_vec()),]
    );
}
