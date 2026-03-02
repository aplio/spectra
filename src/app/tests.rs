use std::collections::HashMap;
use std::io;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::style::Color;

use crate::command_history::CommandHistory;
use crate::input::{CommandAction, KeyMapper};
use crate::ipc::protocol::{CommandRequest, CommandResult, CommandSplitAxis};
use crate::session::manager::{PaneTerminalEvent, SessionManager, SessionOptions};
use crate::session::pane::{FakeBackend, PaneBackend};
use crate::session::pty_backend::{PaneFactory, PaneSpawnConfig};
use crate::session::terminal_state::TerminalEvent;
use crate::storage::DataStore;

use super::{
    App, AppSignal, AttachTarget, InputMode, ManagedSession, RenameTarget, RuntimeUiConfig,
    TreeRowKind, is_closed_pane_error, session_id_for,
};

type RecordedWrites = Arc<Mutex<Vec<(usize, Vec<u8>)>>>;
type ResizeRecords = Arc<Mutex<Vec<(u16, u16)>>>;

struct FakeFactory;

impl PaneFactory for FakeFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(FakeBackend::new(vec![])))
    }
}

struct HistoryFactory;

impl PaneFactory for HistoryFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(FakeBackend::new(vec![
            b"alpha\r\nbeta\r\ngamma".to_vec(),
        ])))
    }
}

#[derive(Debug, Clone, Copy)]
enum WriteBehavior {
    Eio,
    PermissionDenied,
}

struct WriteBehaviorBackend {
    behavior: WriteBehavior,
}

impl PaneBackend for WriteBehaviorBackend {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
        match self.behavior {
            WriteBehavior::Eio => Err(io::Error::from_raw_os_error(5)),
            WriteBehavior::PermissionDenied => {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            }
        }
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

struct WriteBehaviorFactory {
    behavior: WriteBehavior,
}

impl PaneFactory for WriteBehaviorFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(WriteBehaviorBackend {
            behavior: self.behavior,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
struct CloseOnWriteBehavior {
    close_on_ctrl_d: bool,
    close_on_exit_command: bool,
}

impl CloseOnWriteBehavior {
    const CTRL_D: Self = Self {
        close_on_ctrl_d: true,
        close_on_exit_command: false,
    };
    const EXIT_COMMAND: Self = Self {
        close_on_ctrl_d: false,
        close_on_exit_command: true,
    };
}

struct CloseOnWriteBackend {
    behavior: CloseOnWriteBehavior,
    closed: bool,
}

impl PaneBackend for CloseOnWriteBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.behavior.close_on_ctrl_d && bytes == [0x04] {
            self.closed = true;
        }
        if self.behavior.close_on_exit_command && bytes == b"exit\r" {
            self.closed = true;
        }
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn is_closed(&mut self) -> bool {
        self.closed
    }
}

struct CloseOnWriteFactory {
    behavior: CloseOnWriteBehavior,
}

impl PaneFactory for CloseOnWriteFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(CloseOnWriteBackend {
            behavior: self.behavior,
            closed: false,
        }))
    }
}

#[derive(Clone)]
struct RecordingFactory {
    next_backend_id: Arc<AtomicUsize>,
    writes: RecordedWrites,
    initial_output: Vec<Vec<u8>>,
}

impl RecordingFactory {
    fn new(writes: RecordedWrites) -> Self {
        Self::with_output(writes, Vec::new())
    }

    fn with_output(writes: RecordedWrites, initial_output: Vec<Vec<u8>>) -> Self {
        Self {
            next_backend_id: Arc::new(AtomicUsize::new(1)),
            writes,
            initial_output,
        }
    }
}

struct RecordingBackend {
    backend_id: usize,
    writes: RecordedWrites,
    output: Vec<Vec<u8>>,
}

impl PaneBackend for RecordingBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes
            .lock()
            .expect("recording backend lock")
            .push((self.backend_id, bytes.to_vec()));
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.output)
    }
}

impl PaneFactory for RecordingFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        let backend_id = self.next_backend_id.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(RecordingBackend {
            backend_id,
            writes: Arc::clone(&self.writes),
            output: self.initial_output.clone(),
        }))
    }
}

type RecordedSpawnConfigs = Arc<Mutex<Vec<PaneSpawnConfig>>>;
type BackendClosedFlags = Arc<Mutex<HashMap<usize, bool>>>;

#[derive(Clone)]
struct EditorPaneFactory {
    next_backend_id: Arc<AtomicUsize>,
    configs: RecordedSpawnConfigs,
    close_flags: BackendClosedFlags,
}

impl EditorPaneFactory {
    fn new(configs: RecordedSpawnConfigs, close_flags: BackendClosedFlags) -> Self {
        Self {
            next_backend_id: Arc::new(AtomicUsize::new(1)),
            configs,
            close_flags,
        }
    }
}

struct EditorPaneBackend {
    backend_id: usize,
    close_flags: BackendClosedFlags,
}

impl PaneBackend for EditorPaneBackend {
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
        *self
            .close_flags
            .lock()
            .expect("editor close flags lock")
            .get(&self.backend_id)
            .unwrap_or(&false)
    }
}

impl PaneFactory for EditorPaneFactory {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        let backend_id = self.next_backend_id.fetch_add(1, Ordering::Relaxed);
        self.configs
            .lock()
            .expect("editor spawn config lock")
            .push(config.clone());
        self.close_flags
            .lock()
            .expect("editor close flags lock")
            .insert(backend_id, false);
        Ok(Box::new(EditorPaneBackend {
            backend_id,
            close_flags: Arc::clone(&self.close_flags),
        }))
    }
}

fn build_app_for_resize_test() -> App {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(options.clone(), Arc::new(FakeFactory), 80, 24)
        .expect("create session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    }
}

fn build_app_with_resize_recording_backend(
    initial_cols: u16,
    initial_rows: u16,
) -> (App, ResizeRecords) {
    #[derive(Clone)]
    struct ResizeFactory {
        resizes: ResizeRecords,
    }

    struct ResizeBackend {
        resizes: ResizeRecords,
    }

    impl PaneBackend for ResizeBackend {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
            self.resizes.lock().expect("resize lock").push((cols, rows));
            Ok(())
        }

        fn poll_output(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }
    }

    impl PaneFactory for ResizeFactory {
        fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
            Ok(Box::new(ResizeBackend {
                resizes: Arc::clone(&self.resizes),
            }))
        }
    }

    let mut app = build_app_for_resize_test();
    let resizes: ResizeRecords = Arc::new(Mutex::new(Vec::new()));
    let session = SessionManager::with_factory(
        app.session_template.clone(),
        Arc::new(ResizeFactory {
            resizes: Arc::clone(&resizes),
        }),
        initial_cols,
        initial_rows,
    )
    .expect("create resize recording session");
    app.sessions[0].session = session;
    app.view.cols = initial_cols;
    app.view.rows = initial_rows;
    (app, resizes)
}

fn build_app_with_history() -> App {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options.clone(), Arc::new(HistoryFactory), 80, 24)
            .expect("create history session");
    assert!(session.poll_output(), "expected history chunks");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    }
}

fn open_command_palette(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("open command palette");
}

fn type_command_palette_query(app: &mut App, query: &str) {
    for ch in query.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type command palette query");
    }
}

fn expand_all_system_tree_windows(app: &mut App) {
    let window_counts = app
        .sessions
        .iter()
        .map(|managed| managed.session.window_count())
        .collect::<Vec<_>>();
    let InputMode::SystemTree { state } = &mut app.view.input_mode else {
        panic!("expected tree mode");
    };
    for (session_index, window_count) in window_counts.into_iter().enumerate() {
        for window_index in 0..window_count {
            state.expanded_windows.insert(super::TreeWindowKey {
                session_index,
                window_index,
            });
        }
    }
}

fn send_prefix_key_for_client(app: &mut App, client_id: super::ClientId, key: KeyCode) {
    app.handle_key_event_for_client(
        client_id,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    )
    .expect("enter prefix");
    app.handle_key_event_for_client(client_id, KeyEvent::new(key, KeyModifiers::NONE))
        .expect("send prefixed key");
}

fn build_app_with_write_behavior(behavior: WriteBehavior) -> App {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(
        options.clone(),
        Arc::new(WriteBehaviorFactory { behavior }),
        80,
        24,
    )
    .expect("create session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    }
}

fn build_app_with_close_on_write_behavior(behavior: CloseOnWriteBehavior) -> App {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(
        options.clone(),
        Arc::new(CloseOnWriteFactory { behavior }),
        80,
        24,
    )
    .expect("create session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    }
}

fn add_fake_session(app: &mut App, session_name: &str, session_id: &str) {
    let ordinal = app.next_session_ordinal;
    app.next_session_ordinal += 1;
    let options = app
        .session_template
        .clone()
        .with_session_name(session_name.to_string());
    let session =
        SessionManager::with_factory(options, Arc::new(FakeFactory), app.view.cols, app.view.rows)
            .expect("create fake session");

    app.sessions.push(ManagedSession {
        ordinal,
        session_id: session_id.to_string(),
        session,
        window_names: HashMap::new(),
        pane_names: HashMap::new(),
        window_auto_names: HashMap::new(),
        pane_auto_names: HashMap::new(),
        terminal_titles: HashMap::new(),
        cwd_fallbacks: HashMap::new(),
    });
}

fn build_app_with_named_sessions_for_attach() -> App {
    let mut app = build_app_for_resize_test();
    app.sessions[0].session.rename_session("alpha".to_string());
    app.sessions[0].session_id = "alpha-1".to_string();

    app.create_session();
    app.sessions[1].session.rename_session("beta".to_string());
    app.sessions[1].session_id = "beta-2".to_string();
    app.sessions[1]
        .session
        .new_window(80, 24)
        .expect("create second session window");

    app.view.active_session = 0;
    app
}

fn build_recording_app_one_session() -> (App, RecordedWrites) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RecordingFactory::new(Arc::clone(&writes)));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(options.clone(), factory, 80, 24)
        .expect("create recording session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    let app = App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    };

    (app, writes)
}

fn history_output_chunks() -> Vec<Vec<u8>> {
    let mut output = String::new();
    for index in 0..64 {
        output.push_str(&format!("line-{index:03}\r\n"));
    }
    output.push_str("line-END");
    vec![output.into_bytes()]
}

fn colored_history_output_chunks() -> Vec<Vec<u8>> {
    let mut output = String::new();
    for index in 0..64 {
        output.push_str(&format!("\x1b[31mred-{index:03}\x1b[0m\r\n"));
    }
    output.push_str("\x1b[32mgreen-END\x1b[0m");
    vec![output.into_bytes()]
}

fn build_recording_app_with_history() -> (App, RecordedWrites) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RecordingFactory::with_output(
        Arc::clone(&writes),
        history_output_chunks(),
    ));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session = SessionManager::with_factory(options.clone(), factory, 80, 24)
        .expect("create recording history session");
    assert!(session.poll_output(), "expected history output");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    let app = App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    };

    (app, writes)
}

fn build_recording_app_with_output(output: Vec<Vec<u8>>) -> (App, RecordedWrites) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RecordingFactory::with_output(Arc::clone(&writes), output));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(options.clone(), factory, 80, 24)
        .expect("create recording output session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    let app = App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    };

    (app, writes)
}

fn build_recording_app_multi_session() -> (App, RecordedWrites) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RecordingFactory::new(Arc::clone(&writes)));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);

    let mut first_session = SessionManager::with_factory(options.clone(), factory.clone(), 80, 24)
        .expect("create first recording session");
    first_session
        .new_window(80, 24)
        .expect("add second pane to s1");

    let second_options = options.clone().with_session_name("alt");
    let second_session = SessionManager::with_factory(second_options, factory, 80, 24)
        .expect("create second recording session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();

    let app = App {
        sessions: vec![
            ManagedSession {
                ordinal: 1,
                session_id: "main-1".to_string(),
                session: first_session,
                window_names: HashMap::new(),
                pane_names: HashMap::new(),
                window_auto_names: HashMap::new(),
                pane_auto_names: HashMap::new(),
                terminal_titles: HashMap::new(),
                cwd_fallbacks: HashMap::new(),
            },
            ManagedSession {
                ordinal: 2,
                session_id: "alt-2".to_string(),
                session: second_session,
                window_names: HashMap::new(),
                pane_names: HashMap::new(),
                window_auto_names: HashMap::new(),
                pane_auto_names: HashMap::new(),
                terminal_titles: HashMap::new(),
                cwd_fallbacks: HashMap::new(),
            },
        ],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 3,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: None,
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    };

    (app, writes)
}

#[test]
fn tick_queues_tmux_passthrough_for_each_attached_client_on_session() {
    let (mut app, _) = build_recording_app_with_output(vec![
        b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\".to_vec(),
    ]);
    let remote_client_id = 42;
    app.register_client(remote_client_id, 80, 24);

    app.tick();

    assert_eq!(
        app.take_pending_passthrough_ansi_for_client(super::LOCAL_CLIENT_ID),
        vec!["\x1b]52;c;aGVsbG8=\x07".to_string()]
    );
    assert_eq!(
        app.take_pending_passthrough_ansi_for_client(remote_client_id),
        vec!["\x1b]52;c;aGVsbG8=\x07".to_string()]
    );
}

#[test]
fn disabling_passthrough_stops_forwarding_tmux_wrapped_sequences() {
    let (mut app, _) = build_recording_app_with_output(vec![
        b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\".to_vec(),
    ]);
    app.current_session_mut().set_allow_passthrough(false);

    app.tick();

    assert!(
        app.take_pending_passthrough_ansi_for_client(super::LOCAL_CLIENT_ID)
            .is_empty()
    );
}

fn build_editor_command_app() -> (App, RecordedSpawnConfigs, BackendClosedFlags) {
    let configs = Arc::new(Mutex::new(Vec::new()));
    let close_flags = Arc::new(Mutex::new(HashMap::new()));
    let factory = Arc::new(EditorPaneFactory::new(
        Arc::clone(&configs),
        Arc::clone(&close_flags),
    ));
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let session = SessionManager::with_factory(options.clone(), factory, 80, 24)
        .expect("create editor command session");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempdir.path().to_path_buf();
    let app = App {
        sessions: vec![ManagedSession {
            ordinal: 1,
            session_id: "main-1".to_string(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        }],
        view: super::ClientViewState {
            keys: KeyMapper::new(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: 80,
            rows: 24,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        },
        next_session_ordinal: 2,
        session_template: options,
        key_template: KeyMapper::new(),
        status_format: super::DEFAULT_STATUS_FORMAT.to_string(),
        status_style: super::default_status_style(),
        hooks: crate::config::HooksConfig::default(),
        editor_command: Some("vim".to_string()),
        editor_pane_close_targets: Vec::new(),
        store: DataStore::from_base_dir_for_tests(data_dir.clone()),
        command_history: CommandHistory::new_with_data_dir(data_dir),
        started_unix: 1,
        mouse_enabled: false,
        client_focus_profiles: HashMap::new(),
        client_identities: HashMap::from([(
            super::LOCAL_CLIENT_ID,
            super::LOCAL_CLIENT_FOCUS_IDENTITY.to_string(),
        )]),
        active_client_id: super::LOCAL_CLIENT_ID,
        inactive_client_states: HashMap::new(),
        should_quit: false,
        needs_render: false,
        needs_full_clear: false,
        renderer: crate::ui::render::FrameRenderer::new(),
    };

    (app, configs, close_flags)
}

fn take_recorded_writes(writes: &RecordedWrites) -> Vec<(usize, Vec<u8>)> {
    std::mem::take(&mut *writes.lock().expect("recording writes lock"))
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn restore_from_runtime_state_recovers_multi_session_layout_and_focus() {
    let mut app = build_app_for_resize_test();
    app.sessions[0].session.rename_session("alpha".to_string());
    app.sessions[0]
        .session
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split first session");
    app.sessions[0]
        .session
        .new_window(80, 24)
        .expect("new window in first session");
    app.sessions[0]
        .session
        .focus_window_number(2)
        .expect("focus second window in first session");

    let first_entries = app.sessions[0].session.window_entries();
    assert_eq!(first_entries.len(), 2);
    let first_window_id = first_entries[0].window_id;
    let second_window_id = first_entries[1].window_id;
    let second_window_pane = first_entries[1].pane_id;
    app.sessions[0]
        .window_names
        .insert(first_window_id, "editor".to_string());
    app.sessions[0]
        .window_names
        .insert(second_window_id, "logs".to_string());
    app.sessions[0]
        .pane_names
        .insert(second_window_pane, "tail".to_string());

    app.create_session();
    app.sessions[1].session.rename_session("beta".to_string());
    app.sessions[1]
        .session
        .new_window(80, 24)
        .expect("new window in second session");
    app.sessions[1]
        .session
        .focus_window_number(2)
        .expect("focus second window in second session");
    let second_entries = app.sessions[1].session.window_entries();
    let second_focus_pane = second_entries[1].pane_id;
    let second_focus_window_id = second_entries[1].window_id;
    app.sessions[1]
        .session
        .focus_pane_id(second_focus_pane)
        .expect("focus second session pane");
    app.sessions[1]
        .window_names
        .insert(second_focus_window_id, "build".to_string());
    app.sessions[1]
        .pane_names
        .insert(second_focus_pane, "runner".to_string());

    app.view.active_session = 1;
    app.persist_active_session_info();

    let restored = App::restore_from_runtime_state(
        &app.store,
        app.started_unix,
        app.session_template.clone(),
        RuntimeUiConfig {
            keys: app.view.keys.clone(),
            mouse_enabled: app.mouse_enabled,
            status_format: app.status_format.clone(),
            status_style: app.status_style,
            hooks: app.hooks.clone(),
            editor_command: app.editor_command.clone(),
        },
        app.view.cols,
        app.view.rows,
    )
    .expect("restore app")
    .expect("runtime state should restore");

    assert_eq!(restored.sessions.len(), 2);
    assert_eq!(restored.view.active_session, 1);
    assert_eq!(restored.next_session_ordinal, app.next_session_ordinal);
    assert_eq!(restored.sessions[0].session.session_name(), "alpha");
    assert_eq!(restored.sessions[1].session.session_name(), "beta");
    assert_eq!(restored.sessions[0].session.window_count(), 2);
    assert_eq!(restored.sessions[0].session.pane_count(), 3);
    assert_eq!(restored.sessions[1].session.window_count(), 2);
    assert_eq!(restored.sessions[1].session.pane_count(), 2);
    assert_eq!(
        restored.sessions[0].session.focused_window_number(),
        Some(2)
    );
    assert_eq!(
        restored.sessions[1].session.focused_window_number(),
        Some(2)
    );
    assert_eq!(
        restored.sessions[1].session.focused_pane_id(),
        Some(second_focus_pane)
    );
    assert_eq!(
        restored.sessions[0]
            .window_names
            .get(&first_window_id)
            .map(String::as_str),
        Some("editor")
    );
    assert_eq!(
        restored.sessions[0]
            .window_names
            .get(&second_window_id)
            .map(String::as_str),
        Some("logs")
    );
    assert_eq!(
        restored.sessions[0]
            .pane_names
            .get(&second_window_pane)
            .map(String::as_str),
        Some("tail")
    );
    assert_eq!(
        restored.sessions[1]
            .window_names
            .get(&second_focus_window_id)
            .map(String::as_str),
        Some("build")
    );
    assert_eq!(
        restored.sessions[1]
            .pane_names
            .get(&second_focus_pane)
            .map(String::as_str),
        Some("runner")
    );
}

#[test]
fn restore_from_runtime_state_returns_none_on_corrupt_json() {
    let app = build_app_for_resize_test();
    std::fs::create_dir_all(app.store.base_dir()).expect("create runtime state dir");
    std::fs::write(app.store.runtime_state_path(), b"{not-json")
        .expect("write corrupt runtime state");

    let restored = App::restore_from_runtime_state(
        &app.store,
        app.started_unix,
        app.session_template.clone(),
        RuntimeUiConfig {
            keys: app.view.keys.clone(),
            mouse_enabled: app.mouse_enabled,
            status_format: app.status_format.clone(),
            status_style: app.status_style,
            hooks: app.hooks.clone(),
            editor_command: app.editor_command.clone(),
        },
        app.view.cols,
        app.view.rows,
    )
    .expect("restore should not fail on corrupt state");

    assert!(restored.is_none());
}

#[test]
fn restore_from_runtime_state_returns_none_on_invalid_snapshot() {
    let app = build_app_for_resize_test();
    let mut state = app.runtime_state_snapshot();
    state.sessions[0].session.windows.clear();
    std::fs::create_dir_all(app.store.base_dir()).expect("create runtime state dir");
    app.store
        .write_runtime_state(&state)
        .expect("write invalid runtime state");

    let restored = App::restore_from_runtime_state(
        &app.store,
        app.started_unix,
        app.session_template.clone(),
        RuntimeUiConfig {
            keys: app.view.keys.clone(),
            mouse_enabled: app.mouse_enabled,
            status_format: app.status_format.clone(),
            status_style: app.status_style,
            hooks: app.hooks.clone(),
            editor_command: app.editor_command.clone(),
        },
        app.view.cols,
        app.view.rows,
    )
    .expect("restore should not fail on invalid snapshot");

    assert!(restored.is_none());
}

#[test]
fn resize_marks_render_without_full_clear() {
    let mut app = build_app_for_resize_test();

    app.handle_resize_event(100, 40).expect("resize session");

    assert_eq!(app.view.cols, 100);
    assert_eq!(app.view.rows, 40);
    assert!(app.needs_render);
    assert!(!app.needs_full_clear);
}

#[test]
fn local_resize_event_resizes_session_backends() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);

    resizes.lock().expect("resize clear lock").clear();
    app.handle_resize_event(100, 40).expect("resize locally");
    let recorded = resizes.lock().expect("resize read lock");
    assert!(!recorded.is_empty(), "expected resize propagation");
    let expected = (100u16, 39u16);
    assert!(
        recorded.iter().all(|&size| size == expected),
        "expected backend resize to match local viewport, got {recorded:?}"
    );
}

#[test]
fn client_resize_event_resizes_backends_to_max_connected_viewport() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);

    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);
    resizes.lock().expect("resize clear lock").clear();

    app.handle_client_resize_event(1, 120, 40)
        .expect("resize first client");
    app.handle_client_resize_event(2, 90, 20)
        .expect("resize second client");

    let recorded = resizes.lock().expect("resize read lock");
    assert!(!recorded.is_empty(), "expected resize propagation");
    let expected = (120u16, 39u16);
    assert!(
        recorded.iter().all(|&size| size == expected),
        "expected backend resize to stay at max connected viewport, got {recorded:?}"
    );
}

#[test]
fn side_window_tree_toggle_resizes_backends_to_effective_viewport() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);

    resizes.lock().expect("resize clear lock").clear();
    let _ = app.handle_action(CommandAction::SideWindowTree);
    assert!(app.side_window_tree_is_open(), "sidebar should open");
    let open_recorded = resizes.lock().expect("resize read lock");
    let open_expected = (58u16, 23u16);
    assert!(
        open_recorded.iter().all(|&size| size == open_expected),
        "expected sidebar-open resize to use effective viewport, got {open_recorded:?}"
    );
    drop(open_recorded);

    resizes.lock().expect("resize clear lock").clear();
    let _ = app.handle_action(CommandAction::SideWindowTree);
    assert!(!app.side_window_tree_is_open(), "sidebar should close");
    let close_recorded = resizes.lock().expect("resize read lock");
    let close_expected = (80u16, 23u16);
    assert!(
        close_recorded.iter().all(|&size| size == close_expected),
        "expected sidebar-close resize to restore full viewport, got {close_recorded:?}"
    );
}

#[test]
fn resize_event_uses_effective_viewport_when_sidebar_open() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);

    let _ = app.handle_action(CommandAction::SideWindowTree);
    assert!(app.side_window_tree_is_open(), "sidebar should open");
    resizes.lock().expect("resize clear lock").clear();

    app.handle_resize_event(100, 30)
        .expect("resize while sidebar open");
    let recorded = resizes.lock().expect("resize read lock");
    let expected = (72u16, 29u16);
    assert!(
        recorded.iter().all(|&size| size == expected),
        "expected resize to use sidebar-adjusted viewport, got {recorded:?}"
    );
}

#[test]
fn new_window_while_sidebar_open_uses_effective_viewport() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);

    let _ = app.handle_action(CommandAction::SideWindowTree);
    assert!(app.side_window_tree_is_open(), "sidebar should open");
    resizes.lock().expect("resize clear lock").clear();

    let _ = app.handle_action(CommandAction::NewWindow);
    let recorded = resizes.lock().expect("resize read lock");
    let expected = (58u16, 23u16);
    assert!(
        recorded.iter().all(|&size| size == expected),
        "expected layout actions to use sidebar-adjusted viewport, got {recorded:?}"
    );
}

#[test]
fn mixed_client_sidebar_states_resize_to_max_effective_viewport() {
    let (mut app, resizes) = build_app_with_resize_recording_backend(80, 24);
    app.register_client(1, 120, 40);
    app.register_client(2, 100, 30);
    resizes.lock().expect("resize clear lock").clear();

    let _ = app.handle_action_for_client(1, CommandAction::SideWindowTree);
    let recorded = resizes.lock().expect("resize read lock");
    let expected = (100u16, 39u16);
    assert!(
        recorded.iter().all(|&size| size == expected),
        "expected max effective viewport sizing across clients, got {recorded:?}"
    );
}

#[test]
fn render_snapshot_uses_per_client_viewport_size() {
    let mut app = build_app_for_resize_test();
    app.register_client(1, 90, 30);
    app.register_client(2, 120, 20);
    app.request_render(false);

    let first = app
        .render_snapshot_for_client(1)
        .expect("snapshot for client 1");
    let second = app
        .render_snapshot_for_client(2)
        .expect("snapshot for client 2");

    assert_eq!((first.cols, first.rows), (90, 30));
    assert_eq!((second.cols, second.rows), (120, 20));
    app.finish_render_cycle();
}

#[test]
fn prefix_state_is_isolated_per_client() {
    let mut app = build_app_for_resize_test();
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);

    let signal = app
        .handle_key_event_for_client(1, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix for client 1");
    assert_eq!(signal, AppSignal::None);

    let client_one = app
        .render_snapshot_for_client(1)
        .expect("client 1 snapshot after prefix");
    let client_two = app
        .render_snapshot_for_client(2)
        .expect("client 2 snapshot after prefix");
    assert!(client_one.status_line.contains("prefix on"));
    assert!(client_two.status_line.contains("prefix off"));
    app.finish_render_cycle();
}

#[test]
fn mode_state_is_isolated_per_client() {
    let mut app = build_app_for_resize_test();
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);

    app.handle_key_event_for_client(1, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix for client 1");
    app.handle_key_event_for_client(1, KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("open cursor mode for client 1");

    let client_one = app
        .render_snapshot_for_client(1)
        .expect("client 1 snapshot in cursor mode");
    let client_two = app
        .render_snapshot_for_client(2)
        .expect("client 2 snapshot stays normal");
    assert!(client_one.status_line.contains("cursor mode"));
    assert!(!client_two.status_line.contains("cursor mode"));
    app.finish_render_cycle();
}

#[test]
fn lock_state_is_isolated_per_client() {
    let mut app = build_app_for_resize_test();
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);

    app.handle_action_for_client(1, CommandAction::EnterLockMode);

    let client_one = app
        .render_snapshot_for_client(1)
        .expect("client 1 snapshot while locked");
    let client_two = app
        .render_snapshot_for_client(2)
        .expect("client 2 snapshot while unlocked");
    assert!(client_one.status_line.contains("LOCK"));
    assert!(!client_two.status_line.contains("LOCK"));
    app.finish_render_cycle();
}

#[test]
fn active_session_is_isolated_per_client() {
    let mut app = build_app_with_named_sessions_for_attach();
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);

    let alpha = AttachTarget::parse("alpha-1").expect("parse alpha target");
    let beta = AttachTarget::parse("beta-2").expect("parse beta target");
    app.apply_attach_target_for_client(1, &alpha)
        .expect("attach client 1 to alpha");
    app.apply_attach_target_for_client(2, &beta)
        .expect("attach client 2 to beta");

    let client_one = app
        .render_snapshot_for_client(1)
        .expect("client 1 snapshot after attach");
    let client_two = app
        .render_snapshot_for_client(2)
        .expect("client 2 snapshot after attach");

    assert!(client_one.status_line.contains("session 1/2:alpha"));
    assert!(client_two.status_line.contains("session 2/2:beta"));
    app.finish_render_cycle();
}

#[test]
fn focus_prev_next_pane_history_is_non_wrapping() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");

    let frame = app.current_session().frame(app.view.cols, app.view.rows);
    let left = frame
        .panes
        .iter()
        .min_by_key(|pane| pane.rect.x)
        .expect("left pane")
        .pane_id;
    let right = frame
        .panes
        .iter()
        .max_by_key(|pane| pane.rect.x)
        .expect("right pane")
        .pane_id;

    app.current_session_mut()
        .focus_pane_id(left)
        .expect("focus left");
    app.record_focus_for_active_session();
    app.current_session_mut()
        .focus_pane_id(right)
        .expect("focus right");
    app.record_focus_for_active_session();

    let _ = app.handle_action(CommandAction::FocusPrevPane);
    assert_eq!(app.current_session().focused_pane_id(), Some(left));

    let _ = app.handle_action(CommandAction::FocusPrevPane);
    assert_eq!(app.current_session().focused_pane_id(), Some(left));

    let _ = app.handle_action(CommandAction::FocusNextPane);
    assert_eq!(app.current_session().focused_pane_id(), Some(right));

    let _ = app.handle_action(CommandAction::FocusNextPane);
    assert_eq!(app.current_session().focused_pane_id(), Some(right));
}

#[test]
fn pane_focus_is_isolated_per_client_in_same_session() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);

    send_prefix_key_for_client(&mut app, 1, KeyCode::Left);

    let client_one_focus =
        app.with_client_context(1, |app| app.current_session().focused_pane_id());
    let client_two_focus =
        app.with_client_context(2, |app| app.current_session().focused_pane_id());
    assert_ne!(client_one_focus, client_two_focus);
}

#[test]
fn pane_history_prunes_closed_pane_entries() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    send_prefix_key_for_client(&mut app, super::LOCAL_CLIENT_ID, KeyCode::Left);
    send_prefix_key_for_client(&mut app, super::LOCAL_CLIENT_ID, KeyCode::Right);

    let _ = app.handle_action(CommandAction::ClosePane);
    let focused_after_close = app.current_session().focused_pane_id();
    let _ = app.handle_action(CommandAction::FocusPrevPane);
    assert_eq!(app.current_session().focused_pane_id(), focused_after_close);
    let _ = app.handle_action(CommandAction::FocusNextPane);
    assert_eq!(app.current_session().focused_pane_id(), focused_after_close);
}

#[test]
fn restore_from_runtime_state_restores_client_focus_profiles() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);
    app.register_client_identity(1, Some("client-a".to_string()));
    app.register_client_identity(2, Some("client-b".to_string()));

    send_prefix_key_for_client(&mut app, 1, KeyCode::Left);
    send_prefix_key_for_client(&mut app, 2, KeyCode::Right);
    app.persist_active_session_info();

    let mut restored = App::restore_from_runtime_state(
        &app.store,
        app.started_unix,
        app.session_template.clone(),
        RuntimeUiConfig {
            keys: app.view.keys.clone(),
            mouse_enabled: app.mouse_enabled,
            status_format: app.status_format.clone(),
            status_style: app.status_style,
            hooks: app.hooks.clone(),
            editor_command: app.editor_command.clone(),
        },
        app.view.cols,
        app.view.rows,
    )
    .expect("restore app")
    .expect("runtime state should restore");

    restored.register_client(1, 80, 24);
    restored.register_client(2, 80, 24);
    restored.register_client_identity(1, Some("client-a".to_string()));
    restored.register_client_identity(2, Some("client-b".to_string()));

    let client_one_focus =
        restored.with_client_context(1, |app| app.current_session().focused_pane_id());
    let client_two_focus =
        restored.with_client_context(2, |app| app.current_session().focused_pane_id());
    assert_ne!(client_one_focus, client_two_focus);
}

#[test]
fn attach_target_overrides_restored_profile_for_client() {
    let mut app = build_app_with_named_sessions_for_attach();
    app.register_client(1, 80, 24);
    app.register_client_identity(1, Some("client-attach".to_string()));
    app.persist_active_session_info();

    let mut restored = App::restore_from_runtime_state(
        &app.store,
        app.started_unix,
        app.session_template.clone(),
        RuntimeUiConfig {
            keys: app.view.keys.clone(),
            mouse_enabled: app.mouse_enabled,
            status_format: app.status_format.clone(),
            status_style: app.status_style,
            hooks: app.hooks.clone(),
            editor_command: app.editor_command.clone(),
        },
        app.view.cols,
        app.view.rows,
    )
    .expect("restore app")
    .expect("runtime state should restore");

    restored.register_client(1, 80, 24);
    restored.register_client_identity(1, Some("client-attach".to_string()));
    let target = AttachTarget::parse("s2").expect("parse target");
    restored
        .apply_attach_target_for_client(1, &target)
        .expect("apply attach target for client");

    let active_session = restored.with_client_context(1, |app| app.view.active_session);
    assert_eq!(active_session, 1);
}

#[test]
fn session_id_is_sanitized() {
    assert_eq!(session_id_for("Dev Session", 3), "dev_session-3");
}

#[test]
fn attach_target_resolves_session_by_id() {
    let mut app = build_app_with_named_sessions_for_attach();
    let target = AttachTarget::parse("beta-2").expect("parse target");

    app.apply_attach_target(&target)
        .expect("apply attach target");

    assert_eq!(app.view.active_session, 1);
}

#[test]
fn attach_target_resolves_session_by_alias() {
    let mut app = build_app_with_named_sessions_for_attach();
    let target = AttachTarget::parse("s2").expect("parse target");

    app.apply_attach_target(&target)
        .expect("apply attach target");

    assert_eq!(app.view.active_session, 1);
}

#[test]
fn attach_target_resolves_session_by_exact_name() {
    let mut app = build_app_with_named_sessions_for_attach();
    let target = AttachTarget::parse("beta").expect("parse target");

    app.apply_attach_target(&target)
        .expect("apply attach target");

    assert_eq!(app.view.active_session, 1);
}

#[test]
fn attach_target_rejects_ambiguous_session_name() {
    let mut app = build_app_with_named_sessions_for_attach();
    app.sessions[0].session.rename_session("dup".to_string());
    app.sessions[1].session.rename_session("dup".to_string());

    let target = AttachTarget::parse("dup").expect("parse target");
    let err = app
        .apply_attach_target(&target)
        .expect_err("ambiguous name should fail");

    assert!(err.contains("ambiguous"));
}

#[test]
fn attach_target_rejects_missing_session_window_and_pane() {
    let mut app = build_app_with_named_sessions_for_attach();

    let missing_session = AttachTarget::parse("missing").expect("parse target");
    let err = app
        .apply_attach_target(&missing_session)
        .expect_err("missing session should fail");
    assert!(err.contains("not found"));

    let missing_window = AttachTarget::parse("alpha:99").expect("parse target");
    let err = app
        .apply_attach_target(&missing_window)
        .expect_err("missing window should fail");
    assert!(err.contains("window"));

    let missing_pane = AttachTarget::parse("alpha:1.99").expect("parse target");
    let err = app
        .apply_attach_target(&missing_pane)
        .expect_err("missing pane should fail");
    assert!(err.contains("pane"));

    let missing_pane_index = AttachTarget::parse("alpha:1.i9").expect("parse target");
    let err = app
        .apply_attach_target(&missing_pane_index)
        .expect_err("missing pane index should fail");
    assert!(err.contains("pane index"));
}

#[test]
fn attach_target_rejects_pane_window_mismatch() {
    let mut app = build_app_with_named_sessions_for_attach();
    let entries = app.sessions[1].session.window_entries();
    assert_eq!(entries.len(), 2);

    let first_window = entries[0].index;
    let second_pane = entries[1].pane_id;
    let target = AttachTarget::parse(&format!("beta-2:w{first_window}.p{second_pane}"))
        .expect("parse target");
    let err = app
        .apply_attach_target(&target)
        .expect_err("pane/window mismatch should fail");

    assert!(err.contains("not in window"));
}

#[test]
fn attach_target_session_window_pane_updates_focus_and_render_flags() {
    let mut app = build_app_with_named_sessions_for_attach();
    let entries = app.sessions[1].session.window_entries();
    assert_eq!(entries.len(), 2);
    let target_window = entries[1].index;
    let target_pane = entries[1].pane_id;

    app.needs_render = false;
    app.needs_full_clear = false;

    let target =
        AttachTarget::parse(&format!("s2:w{target_window}.p{target_pane}")).expect("parse target");
    app.apply_attach_target(&target).expect("apply target");

    assert_eq!(app.view.active_session, 1);
    assert_eq!(
        app.sessions[1].session.focused_window_number(),
        Some(target_window)
    );
    assert_eq!(app.sessions[1].session.focused_pane_id(), Some(target_pane));
    assert!(app.needs_render);
    assert!(app.needs_full_clear);
}

#[test]
fn attach_target_supports_window_local_pane_index() {
    let mut app = build_app_with_named_sessions_for_attach();
    app.sessions[0]
        .session
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split first session window");
    let target_pane = app.sessions[0]
        .session
        .pane_ids_for_window_number(1)
        .expect("pane ids for window 1")[1];
    let target = AttachTarget::parse("alpha:w1.i2").expect("parse target");

    app.apply_attach_target(&target)
        .expect("apply attach target");

    assert_eq!(app.view.active_session, 0);
    assert_eq!(app.sessions[0].session.focused_window_number(), Some(1));
    assert_eq!(app.sessions[0].session.focused_pane_id(), Some(target_pane));
}

#[test]
fn execute_new_session_adds_one_session() {
    let mut app = build_app_for_resize_test();
    assert_eq!(app.sessions.len(), 1);

    let result = app
        .execute_command(CommandRequest::NewSession)
        .expect("execute new-session");

    assert_eq!(app.sessions.len(), 2);
    assert_eq!(app.view.active_session, 1);
    match result {
        CommandResult::Message { message } => {
            assert!(message.contains("session created"));
        }
        _ => panic!("expected message result"),
    }
}

#[test]
fn execute_kill_last_session_sets_shutdown() {
    let mut app = build_app_for_resize_test();

    let result = app
        .execute_command(CommandRequest::KillSession {
            target: Some("s1".to_string()),
        })
        .expect("execute kill-session");

    assert!(app.should_quit);
    match result {
        CommandResult::Message { message } => {
            assert!(message.contains("server shutting down"));
        }
        _ => panic!("expected message result"),
    }
}

#[test]
fn execute_ls_returns_session_entries() {
    let mut app = build_app_for_resize_test();
    app.execute_command(CommandRequest::NewSession)
        .expect("create second session");

    let result = app.execute_command(CommandRequest::Ls).expect("execute ls");
    match result {
        CommandResult::SessionList { sessions } => {
            assert_eq!(sessions.len(), 2);
            assert_eq!(sessions[0].alias, "s1");
            assert_eq!(sessions[1].alias, "s2");
            assert!(sessions[1].active);
        }
        _ => panic!("expected session list result"),
    }
}

#[test]
fn execute_split_window_adds_pane() {
    let mut app = build_app_for_resize_test();
    let before = app.current_session().pane_count();

    app.execute_command(CommandRequest::SplitWindow {
        target: None,
        axis: CommandSplitAxis::Horizontal,
    })
    .expect("execute split-window");

    assert_eq!(app.current_session().pane_count(), before + 1);
}

#[test]
fn execute_source_file_reloads_keymap_and_shell_toggles() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reload.toml");
    std::fs::write(
        &path,
        r#"
prefix = "C-a"

[shell]
suppress_prompt_eol_marker = false

[terminal]
allow_passthrough = false
"#,
    )
    .expect("write config");

    let result = app
        .execute_command(CommandRequest::SourceFile {
            path: Some(path.display().to_string()),
        })
        .expect("source-file succeeds");
    match result {
        CommandResult::Message { message } => {
            assert!(message.contains("config reloaded"));
        }
        _ => panic!("expected message result"),
    }

    assert!(!app.session_template.suppress_prompt_eol_marker);
    assert!(!app.current_session().suppress_prompt_eol_marker());
    assert!(!app.session_template.allow_passthrough);
    assert!(!app.current_session().allow_passthrough());

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("new prefix key");
    assert!(app.view.keys.prefix_active());
}

#[test]
fn execute_source_file_reports_parse_errors() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("broken.toml");
    std::fs::write(&path, "prefix = [").expect("write invalid toml");

    let err = app
        .execute_command(CommandRequest::SourceFile {
            path: Some(path.display().to_string()),
        })
        .expect_err("source-file should fail for invalid toml");

    assert!(err.contains("source-file failed"));
}

#[test]
fn create_default_config_writes_missing_file() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("spectra").join("config.toml");

    let message = app
        .create_default_config_at_path(&path)
        .expect("create-default writes config");
    assert!(message.contains("config written:"));

    let written = std::fs::read_to_string(&path).expect("read written config");
    let cfg: crate::config::AppConfig = toml::from_str(&written).expect("parse written config");
    assert!(cfg.prefix.is_none());
    assert!(cfg.session_name.is_none());
    assert!(cfg.initial_command.is_none());
    assert!(cfg.shell.suppress_prompt_eol_marker);
    assert!(cfg.terminal.allow_passthrough);
    assert!(!cfg.mouse.enabled);
    assert!(cfg.status.format.is_none());
    assert!(cfg.status.background.is_none());
    assert!(cfg.status.foreground.is_none());
    assert!(cfg.prefix_bindings.is_empty());
    assert!(cfg.global_bindings.is_empty());
    assert!(app.session_template.suppress_prompt_eol_marker);
    assert!(app.session_template.allow_passthrough);
    assert!(app.current_session().allow_passthrough());

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("default prefix key");
    assert!(app.view.keys.prefix_active());
}

#[test]
fn create_default_config_preserves_existing_values_and_fills_missing() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("spectra").join("config.toml");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("create config dir");
    std::fs::write(
        &path,
        r#"
prefix = "C-a"

[shell]
suppress_prompt_eol_marker = false

[mouse]
enabled = true

[terminal]
allow_passthrough = false
"#,
    )
    .expect("write partial config");

    app.create_default_config_at_path(&path)
        .expect("create-default should merge");

    let written = std::fs::read_to_string(&path).expect("read merged config");
    let cfg: crate::config::AppConfig = toml::from_str(&written).expect("parse merged config");
    assert_eq!(cfg.prefix.as_deref(), Some("C-a"));
    assert!(!cfg.shell.suppress_prompt_eol_marker);
    assert!(cfg.mouse.enabled);
    assert!(!cfg.terminal.allow_passthrough);
    assert!(cfg.status.format.is_none());
    assert!(cfg.prefix_bindings.is_empty());
    assert!(cfg.global_bindings.is_empty());
    assert!(!app.session_template.suppress_prompt_eol_marker);
    assert!(!app.session_template.allow_passthrough);
    assert!(!app.current_session().allow_passthrough());
    assert!(app.mouse_enabled);

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("updated prefix key");
    assert!(app.view.keys.prefix_active());
}

#[test]
fn create_default_config_keeps_invalid_existing_file() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("spectra").join("config.toml");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("create config dir");
    let invalid = "prefix = [";
    std::fs::write(&path, invalid).expect("write invalid config");

    let err = app
        .create_default_config_at_path(&path)
        .expect_err("create-default should fail on invalid toml");
    let after = std::fs::read_to_string(&path).expect("read config after failed write");

    assert!(err.contains("config parse failed:"));
    assert_eq!(after, invalid);
}

#[test]
fn command_palette_includes_create_default_config() {
    let entries = App::command_palette_entries();
    assert!(entries.iter().any(|entry| {
        entry.id == "config.create_default" && entry.action == CommandAction::CreateDefaultConfig
    }));
}

#[test]
fn command_palette_includes_peek_all_windows() {
    let entries = App::command_palette_entries();
    assert!(entries.iter().any(|entry| {
        entry.id == "session.peek_all_windows" && entry.action == CommandAction::PeekAllWindows
    }));
}

#[test]
fn command_palette_includes_enter_cursor_mode() {
    let entries = App::command_palette_entries();
    assert!(entries.iter().any(|entry| {
        entry.id == "cursor-mode.enter" && entry.action == CommandAction::EnterCursorMode
    }));
}

#[test]
fn command_palette_includes_leave_cursor_mode() {
    let entries = App::command_palette_entries();
    assert!(entries.iter().any(|entry| {
        entry.id == "cursor-mode.leave" && entry.action == CommandAction::LeaveCursorMode
    }));
}

#[test]
fn status_template_replaces_tokens() {
    let mut app = build_app_for_resize_test();
    app.status_format = "s={session_index}/{session_count}:{session_name}:{session_id} w={window_index}/{window_count}:{window_id} p={pane_index}/{pane_count}:{pane_id} pref={prefix}{lock}{zoom}{sync}{mouse}{message}".to_string();
    app.mouse_enabled = true;
    app.view.locked_input = true;
    app.set_message("hello", Duration::from_secs(5));
    let _ = app.handle_action(CommandAction::ToggleZoom);
    let _ = app.handle_action(CommandAction::ToggleSynchronizePanes);

    let status = app.status_line();
    assert!(status.contains("s=1/1:main:main-1"));
    assert!(status.contains("w=1/1:1"));
    assert!(status.contains("p=1/1:1"));
    assert!(status.contains("pref=off"));
    assert!(status.contains("LOCK"));
    assert!(status.contains("ZOOM"));
    assert!(status.contains("SYNC"));
    assert!(status.contains("MOUSE"));
    assert!(status.contains("hello"));
    assert!(!status.contains("{session_index}"));
}

#[test]
fn status_style_config_uses_defaults_and_hex_overrides() {
    let defaults = super::status_style_from_config(&crate::config::StatusConfig::default());
    assert_eq!(defaults.bg, Some(super::DEFAULT_STATUS_BG));
    assert_eq!(defaults.fg, Some(super::DEFAULT_STATUS_FG));

    let custom = super::status_style_from_config(&crate::config::StatusConfig {
        format: None,
        background: Some("#112233".to_string()),
        foreground: Some("#ABCDEF".to_string()),
    });
    assert_eq!(
        custom.bg,
        Some(crossterm::style::Color::Rgb {
            r: 0x11,
            g: 0x22,
            b: 0x33
        })
    );
    assert_eq!(
        custom.fg,
        Some(crossterm::style::Color::Rgb {
            r: 0xAB,
            g: 0xCD,
            b: 0xEF
        })
    );
}

#[test]
fn status_style_config_ignores_invalid_hex() {
    let style = super::status_style_from_config(&crate::config::StatusConfig {
        format: None,
        background: Some("not-a-color".to_string()),
        foreground: Some("#12345".to_string()),
    });
    assert_eq!(style.bg, Some(super::DEFAULT_STATUS_BG));
    assert_eq!(style.fg, Some(super::DEFAULT_STATUS_FG));
}

#[test]
fn session_created_hook_executes_with_env_context() {
    let mut app = build_app_for_resize_test();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("hook.out");
    app.hooks.session_created = Some(format!(
        "printf '%s:%s' \"$SPECTRA_HOOK_EVENT\" \"$SPECTRA_SESSION_ID\" > '{}'",
        out.display()
    ));

    let session_id = app.create_session_internal().expect("create session");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut content = None;
    while Instant::now() < deadline {
        if let Ok(read) = std::fs::read_to_string(&out) {
            content = Some(read);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let expected = format!("session_created:{session_id}");
    assert_eq!(content.as_deref(), Some(expected.as_str()));
}

#[test]
fn hook_command_failure_is_logged_without_interrupting_flow() {
    let mut app = build_app_for_resize_test();
    app.hooks.session_created = Some("exit 9".to_string());

    let session_id = app.create_session_internal().expect("create session");
    let log_path = app.store.session_dir(&session_id).join("session.log");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_failure = false;
    while Instant::now() < deadline {
        if let Ok(log) = std::fs::read_to_string(&log_path)
            && log.contains("hook session_created failed")
        {
            saw_failure = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(saw_failure);
    assert!(!app.should_quit);
}

#[test]
fn execute_select_window_and_pane_updates_focus() {
    let mut app = build_app_for_resize_test();
    app.execute_command(CommandRequest::NewWindow { target: None })
        .expect("new window");
    let entries = app.current_session().window_entries();
    let second_pane = entries[1].pane_id;

    app.execute_command(CommandRequest::SelectWindow {
        target: None,
        window: 1,
    })
    .expect("select window");
    assert_eq!(app.current_session().focused_window_number(), Some(1));

    app.execute_command(CommandRequest::SelectPane {
        target: None,
        pane: second_pane,
    })
    .expect("select pane");
    assert_eq!(app.current_session().focused_pane_id(), Some(second_pane));
}

#[test]
fn execute_send_keys_defaults_to_focused_pane() {
    let (mut app, writes) = build_recording_app_one_session();

    let result = app
        .execute_command(CommandRequest::SendKeys {
            target: None,
            all: false,
            text: "echo hi".to_string(),
        })
        .expect("send keys");
    assert_eq!(
        result,
        CommandResult::Message {
            message: "keys sent to 1 pane(s)".to_string(),
        }
    );

    assert_eq!(
        take_recorded_writes(&writes),
        vec![(1, b"echo hi".to_vec())]
    );
}

#[test]
fn execute_send_keys_targets_session_scope() {
    let (mut app, writes) = build_recording_app_one_session();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");

    app.execute_command(CommandRequest::SendKeys {
        target: Some(AttachTarget::parse("s1").expect("parse target")),
        all: false,
        text: "pwd".to_string(),
    })
    .expect("send keys");

    assert_eq!(
        take_recorded_writes(&writes),
        vec![(1, b"pwd".to_vec()), (2, b"pwd".to_vec())]
    );
}

#[test]
fn execute_send_keys_targets_window_scope() {
    let (mut app, writes) = build_recording_app_one_session();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");

    app.execute_command(CommandRequest::SendKeys {
        target: Some(AttachTarget::parse("s1:1").expect("parse target")),
        all: false,
        text: "whoami".to_string(),
    })
    .expect("send keys");

    assert_eq!(take_recorded_writes(&writes), vec![(1, b"whoami".to_vec())]);
}

#[test]
fn execute_send_keys_targets_pane_scope() {
    let (mut app, writes) = build_recording_app_one_session();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    let second_pane = app.current_session().window_entries()[1].pane_id;

    app.execute_command(CommandRequest::SendKeys {
        target: Some(AttachTarget::parse(&format!("s1:2.p{second_pane}")).expect("parse target")),
        all: false,
        text: "hostname".to_string(),
    })
    .expect("send keys");

    assert_eq!(
        take_recorded_writes(&writes),
        vec![(second_pane, b"hostname".to_vec())]
    );
}

#[test]
fn execute_send_keys_targets_pane_index_scope() {
    let (mut app, writes) = build_recording_app_one_session();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("create second pane in first window");
    let target_pane = app
        .current_session()
        .pane_ids_for_window_number(1)
        .expect("pane ids for window 1")[1];

    app.execute_command(CommandRequest::SendKeys {
        target: Some(AttachTarget::parse("s1:1.i2").expect("parse target")),
        all: false,
        text: "hostname".to_string(),
    })
    .expect("send keys");

    assert_eq!(
        take_recorded_writes(&writes),
        vec![(target_pane, b"hostname".to_vec())]
    );
}

#[test]
fn execute_send_keys_targets_all_sessions_and_panes() {
    let (mut app, writes) = build_recording_app_multi_session();

    app.execute_command(CommandRequest::SendKeys {
        target: None,
        all: true,
        text: "date".to_string(),
    })
    .expect("send keys");

    assert_eq!(
        take_recorded_writes(&writes),
        vec![
            (1, b"date".to_vec()),
            (2, b"date".to_vec()),
            (3, b"date".to_vec())
        ]
    );
}

#[test]
fn execute_send_keys_rejects_empty_text() {
    let mut app = build_app_for_resize_test();

    let err = app
        .execute_command(CommandRequest::SendKeys {
            target: None,
            all: false,
            text: String::new(),
        })
        .expect_err("empty text should fail");
    assert!(err.contains("send-keys text cannot be empty"));
}

#[test]
fn execute_send_keys_rejects_target_with_all() {
    let mut app = build_app_for_resize_test();

    let err = app
        .execute_command(CommandRequest::SendKeys {
            target: Some(AttachTarget::parse("s1").expect("parse target")),
            all: true,
            text: "echo hi".to_string(),
        })
        .expect_err("target+all should fail");
    assert!(err.contains("--target cannot be used with --all"));
}

#[test]
fn execute_send_keys_does_not_change_focus_or_active_session() {
    let (mut app, writes) = build_recording_app_multi_session();
    app.current_session_mut()
        .focus_window_number(2)
        .expect("focus second window in s1");
    let before_active = app.view.active_session;
    let before_window = app.current_session().focused_window_number();
    let before_pane = app.current_session().focused_pane_id();

    app.execute_command(CommandRequest::SendKeys {
        target: Some(AttachTarget::parse("s2").expect("parse target")),
        all: false,
        text: "id".to_string(),
    })
    .expect("send keys");

    assert_eq!(app.view.active_session, before_active);
    assert_eq!(app.current_session().focused_window_number(), before_window);
    assert_eq!(app.current_session().focused_pane_id(), before_pane);
    assert_eq!(take_recorded_writes(&writes), vec![(3, b"id".to_vec())]);
}

#[test]
fn prefix_w_enters_system_tree_mode() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("handle prefix w");

    assert!(matches!(app.view.input_mode, InputMode::SystemTree { .. }));
}

#[test]
fn prefix_e_toggles_side_window_tree() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("toggle side window tree on");
    assert!(app.side_window_tree_is_open());

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix again");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("toggle side window tree off");
    assert!(!app.side_window_tree_is_open());
}

#[test]
fn side_window_tree_stays_open_across_window_switch() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second window");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first window");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree for window 1");
    assert!(app.side_window_tree_is_open());

    let _ = app.handle_action(CommandAction::NextWindow);
    assert_eq!(app.current_session().focused_window_number(), Some(2));
    assert!(app.side_window_tree_is_open());

    let _ = app.handle_action(CommandAction::PrevWindow);
    assert_eq!(app.current_session().focused_window_number(), Some(1));
    assert!(app.side_window_tree_is_open());
}

#[test]
fn side_window_tree_click_switches_window_and_stays_open() {
    let mut app = build_app_for_resize_test();
    app.mouse_enabled = true;
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second window");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first window");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree");
    app.handle_mouse_event(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 2))
        .expect("click second side-window-tree row");

    assert_eq!(app.current_session().focused_window_number(), Some(2));
    assert!(app.side_window_tree_is_open());
}

#[test]
fn visible_side_window_tree_does_not_capture_navigation_keys() {
    let (mut app, writes) = build_recording_app_one_session();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("send down while sidebar visible");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("send enter while sidebar visible");

    let writes = take_recorded_writes(&writes);
    assert!(
        writes.iter().any(|(_, bytes)| bytes == b"\x1b[B"),
        "down arrow should be forwarded to pane; writes={writes:?}"
    );
    assert!(
        writes.iter().any(|(_, bytes)| bytes == b"\r"),
        "enter should be forwarded to pane; writes={writes:?}"
    );
}

#[test]
fn side_window_tree_overlay_marks_selected_window_with_gt_prefix() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree");

    let snapshot = app.take_render_snapshot().expect("render snapshot");
    let side = snapshot.side_window_tree.expect("sidebar data");
    assert_eq!(side.selected, 0);
    assert_eq!(side.entries, vec!["w1".to_string()]);
}

#[test]
fn side_window_tree_overlay_selected_tracks_focused_window() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second window");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first window");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree");

    let first = app.take_render_snapshot().expect("first render");
    assert_eq!(first.side_window_tree.expect("first sidebar").selected, 0);

    app.current_session_mut()
        .focus_window_number(2)
        .expect("focus second window");
    app.request_render(true);
    let second = app.take_render_snapshot().expect("second render");
    assert_eq!(second.side_window_tree.expect("second sidebar").selected, 1);
}

#[test]
fn hidden_side_window_tree_does_not_capture_keys() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("open side window tree");
    assert!(app.side_window_tree_is_open());
    assert!(app.side_window_tree_overlay().is_some());

    app.handle_resize_event(20, app.view.rows)
        .expect("shrink viewport");
    assert!(app.side_window_tree_is_open());
    assert!(app.side_window_tree_overlay().is_none());

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("send escape while sidebar hidden");
    assert!(
        app.side_window_tree_is_open(),
        "hidden sidebar should not intercept normal keys"
    );
}

#[test]
fn prefix_upper_w_enters_peek_all_windows_mode() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::NONE))
        .expect("handle prefix W");

    assert!(matches!(
        app.view.input_mode,
        InputMode::PeekAllWindows { .. }
    ));
    assert!(app.status_line().contains("peek all panes"));
}

#[test]
fn peek_all_windows_mode_exits_on_any_key_and_restores_focus() {
    let mut app = build_app_for_resize_test();

    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split first window");
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second window");

    let before_window = app.current_session().focused_window_number();
    let before_pane = app.current_session().focused_pane_id();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::NONE))
        .expect("enter peek mode");

    let snapshot = app.take_render_snapshot().expect("peek snapshot");
    assert_eq!(snapshot.frame.panes.len(), 3);
    assert!(matches!(
        app.view.input_mode,
        InputMode::PeekAllWindows { .. }
    ));

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .expect("exit peek mode");

    assert!(matches!(app.view.input_mode, InputMode::Normal));
    assert_eq!(app.current_session().focused_window_number(), before_window);
    assert_eq!(app.current_session().focused_pane_id(), before_pane);
}

#[test]
fn prefix_bracket_enters_cursor_mode() {
    let mut app = build_app_with_history();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("open cursor mode");

    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert!(state.lines.len() >= 3);
    assert!(app.status_line().contains("cursor mode"));
}

#[test]
fn prefix_p_enters_command_palette_mode() {
    let mut app = build_app_for_resize_test();

    open_command_palette(&mut app);

    assert!(matches!(
        app.view.input_mode,
        InputMode::CommandPalette { .. }
    ));
    assert!(app.status_line().contains("command palette"));
}

#[test]
fn command_palette_query_filters_candidates_with_fzf_style_match() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "detach");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "detach");
    assert_eq!(state.text_input.cursor, 6);
    let entries = App::command_palette_entries();
    let recent_command_ids = app.command_history.get_recent_commands(100);
    let candidates = App::command_palette_candidates(state, &entries, &recent_command_ids);
    assert!(!candidates.is_empty(), "expected at least one candidate");
    assert_eq!(
        entries[candidates[0].entry_index].action,
        CommandAction::DetachClient
    );
}

#[test]
fn command_palette_empty_query_prioritizes_recent_history() {
    let mut app = build_app_for_resize_test();
    let history_dir = tempfile::tempdir().expect("history tempdir");
    app.command_history = CommandHistory::new_with_data_dir(history_dir.path().to_path_buf());
    app.command_history
        .record_execution("session.new")
        .expect("record session.new");
    thread::sleep(Duration::from_millis(2));
    app.command_history
        .record_execution("client.detach")
        .expect("record client.detach");

    open_command_palette(&mut app);
    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    let entries = App::command_palette_entries();
    let recent_command_ids = app.command_history.get_recent_commands(100);
    let candidates = App::command_palette_candidates(state, &entries, &recent_command_ids);
    assert!(!candidates.is_empty(), "expected candidates");
    assert_eq!(
        entries[candidates[0].entry_index].id.as_str(),
        "client.detach"
    );
    assert_eq!(
        entries[candidates[1].entry_index].id.as_str(),
        "session.new"
    );
}

#[test]
fn command_palette_left_right_moves_input_cursor() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "abcd");

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("move cursor right");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "abcd");
    assert_eq!(state.text_input.cursor, 3);
}

#[test]
fn command_palette_ctrl_f_b_a_e_moves_input_cursor() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "abcd");

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        .expect("ctrl+f");
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
        .expect("ctrl+b");
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("ctrl+a");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .expect("ctrl+e");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(
        state.text_input.cursor,
        state.text_input.text.chars().count()
    );
}

#[test]
fn command_palette_ctrl_left_right_moves_input_cursor_by_word() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "alpha beta gamma");

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("ctrl+a");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
        .expect("ctrl+right to beta");
    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
        .expect("ctrl+right to gamma");
    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.cursor, 11);

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
        .expect("ctrl+left to beta");
    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.cursor, 6);
}

#[test]
fn command_palette_ctrl_w_and_k_delete_query_segments() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "alpha beta gamma");

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
        .expect("ctrl+left to gamma");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
        .expect("ctrl+w");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "alpha gamma");
    assert_eq!(state.text_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
        .expect("ctrl+k");
    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "alpha ");
    assert_eq!(state.text_input.cursor, 6);
}

#[test]
fn command_palette_ctrl_n_p_j_moves_selection() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .expect("ctrl+n");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("ctrl+j");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .expect("ctrl+p");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.selected, 1);
}

#[test]
fn command_palette_non_press_key_events_are_ignored() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "ab");

    app.handle_key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ))
    .expect("release event should be ignored");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "ab");
    assert_eq!(state.text_input.cursor, 2);
}

#[test]
fn command_palette_ctrl_alt_shortcuts_follow_ctrl_bindings() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);

    app.handle_key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ))
    .expect("ctrl+alt+n should navigate selection");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.selected, 1);
}

#[test]
fn command_palette_unhandled_ctrl_char_does_not_edit_query() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "ab");

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .expect("ctrl+x should be ignored");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "ab");
    assert_eq!(state.text_input.cursor, 2);
}

#[test]
fn command_palette_alt_char_inserts_query_text() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "ab");

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT))
        .expect("alt+x should insert text");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "abx");
    assert_eq!(state.text_input.cursor, 3);
}

#[test]
fn command_palette_query_edit_keeps_selection_stable() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("move selection down");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("move selection down");
    type_command_palette_query(&mut app, "focus");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.selected, 2);
}

#[test]
fn command_palette_ctrl_c_and_q_close_palette() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("ctrl+c close");
    assert_eq!(signal, AppSignal::None);
    assert!(matches!(app.view.input_mode, InputMode::Normal));

    open_command_palette(&mut app);
    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
        .expect("ctrl+q close");
    assert_eq!(signal, AppSignal::None);
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn command_palette_prefix_key_does_not_toggle_prefix_mode() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "ab");

    // Ctrl+J (prefix key) should be handled by command palette (select next),
    // not toggle prefix mode
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("ctrl+j select next");
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("insert q");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "abq");
    assert_eq!(state.text_input.cursor, 3);
    assert!(!app.should_quit);
}

#[test]
fn command_palette_enter_executes_selected_action() {
    let mut app = build_app_for_resize_test();
    let before = app.current_session().pane_count();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "horizontal");

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("execute command");

    assert_eq!(signal, AppSignal::None);
    assert_eq!(app.current_session().pane_count(), before + 1);
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn command_palette_enter_opens_peek_all_windows() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "peek all panes");

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("execute peek all windows command");

    assert_eq!(signal, AppSignal::None);
    assert!(matches!(
        app.view.input_mode,
        InputMode::PeekAllWindows { .. }
    ));
}

#[test]
fn command_palette_enter_can_return_detach_signal() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "detach");

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("execute detach command");

    assert_eq!(signal, AppSignal::DetachClient);
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn command_palette_enter_records_command_history() {
    let mut app = build_app_for_resize_test();
    let history_dir = tempfile::tempdir().expect("history tempdir");
    app.command_history = CommandHistory::new_with_data_dir(history_dir.path().to_path_buf());
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "detach");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("execute detach command");

    let recent = app.command_history.get_recent_commands(1);
    assert_eq!(recent.first().map(String::as_str), Some("client.detach"));
}

#[test]
fn open_pane_buffer_in_editor_creates_window_and_spawns_editor_command() {
    let (mut app, configs, _) = build_editor_command_app();
    let before_windows = app.current_session().window_count();
    let before_panes = app.current_session().pane_count();

    let signal = app.handle_action(CommandAction::OpenPaneBufferInEditor);

    assert_eq!(signal, AppSignal::None);
    assert_eq!(app.current_session().window_count(), before_windows + 1);
    assert_eq!(app.current_session().pane_count(), before_panes + 1);
    assert_eq!(app.editor_pane_close_targets.len(), 1);
    assert_eq!(app.current_session().focused_pane_id(), Some(2));

    let recorded = configs.lock().expect("spawn configs lock");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[1].command.len(), 1);
    assert!(
        recorded[1].command[0].starts_with("vim "),
        "unexpected editor command: {:?}",
        recorded[1].command
    );
    assert!(
        recorded[1].command[0].contains("pane-1-"),
        "expected source pane id in path: {}",
        recorded[1].command[0]
    );

    let scrollback_dir = app
        .store
        .base_dir()
        .join("sessions")
        .join("main-1")
        .join("scrollback");
    let artifact_count = std::fs::read_dir(&scrollback_dir)
        .expect("read scrollback dir")
        .count();
    assert!(
        artifact_count >= 1,
        "expected at least one scrollback artifact"
    );
}

#[test]
fn open_pane_buffer_in_editor_falls_back_to_env_editor() {
    let (mut app, configs, _) = build_editor_command_app();
    app.editor_command = None;

    let prev = std::env::var("EDITOR").ok();
    // SAFETY: test-only, single-threaded access to env var.
    unsafe { std::env::set_var("EDITOR", "nano") };
    let signal = app.handle_action(CommandAction::OpenPaneBufferInEditor);
    match prev {
        Some(v) => unsafe { std::env::set_var("EDITOR", v) },
        None => unsafe { std::env::remove_var("EDITOR") },
    }

    assert_eq!(signal, AppSignal::None);
    assert_eq!(app.current_session().window_count(), 2);
    assert_eq!(app.current_session().pane_count(), 2);

    let recorded = configs.lock().expect("spawn configs lock");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[1].command.len(), 1);
    assert!(
        recorded[1].command[0].starts_with("nano "),
        "unexpected fallback editor command: {:?}",
        recorded[1].command
    );
}

#[test]
fn open_pane_buffer_in_editor_uses_default_vi_when_no_editor() {
    let (mut app, configs, _) = build_editor_command_app();
    app.editor_command = None;

    let prev = std::env::var("EDITOR").ok();
    // SAFETY: test-only, single-threaded access to env var.
    unsafe { std::env::remove_var("EDITOR") };
    let signal = app.handle_action(CommandAction::OpenPaneBufferInEditor);
    if let Some(v) = prev {
        unsafe { std::env::set_var("EDITOR", v) };
    }

    assert_eq!(signal, AppSignal::None);
    assert_eq!(app.current_session().window_count(), 2);
    assert_eq!(app.current_session().pane_count(), 2);

    let recorded = configs.lock().expect("spawn configs lock");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[1].command.len(), 1);
    assert!(
        recorded[1].command[0].starts_with("vi "),
        "unexpected fallback editor command: {:?}",
        recorded[1].command
    );
}

#[test]
fn editor_pane_auto_closes_after_process_exit() {
    let (mut app, _, close_flags) = build_editor_command_app();
    let _ = app.handle_action(CommandAction::OpenPaneBufferInEditor);
    assert_eq!(app.current_session().pane_count(), 2);

    close_flags
        .lock()
        .expect("close flags lock")
        .insert(2, true);
    app.tick();

    assert_eq!(app.current_session().pane_count(), 1);
    assert!(!app.current_session().pane_exists(2));
    assert!(app.editor_pane_close_targets.is_empty());
    assert!(!app.should_quit);
}

#[test]
fn editor_pane_auto_close_works_after_focus_moves_elsewhere() {
    let (mut app, _, close_flags) = build_editor_command_app();
    let _ = app.handle_action(CommandAction::OpenPaneBufferInEditor);
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create focus window");
    assert_eq!(app.current_session().focused_pane_id(), Some(3));

    close_flags
        .lock()
        .expect("close flags lock")
        .insert(2, true);
    app.tick();

    assert!(!app.current_session().pane_exists(2));
    assert_eq!(app.current_session().focused_pane_id(), Some(3));
    assert!(app.editor_pane_close_targets.is_empty());
    assert!(!app.should_quit);
}

#[test]
fn command_palette_escape_closes_without_executing() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("close command palette");

    assert_eq!(signal, AppSignal::None);
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn command_palette_paste_inserts_at_cursor_and_updates_candidates() {
    let mut app = build_app_for_resize_test();
    open_command_palette(&mut app);
    type_command_palette_query(&mut app, "dech");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move cursor left");
    app.handle_paste_text("ta\r\n".to_string())
        .expect("paste query");

    let InputMode::CommandPalette { state } = &app.view.input_mode else {
        panic!("expected command palette mode");
    };
    assert_eq!(state.text_input.text, "detach");
    assert_eq!(state.text_input.cursor, 4);
    let entries = App::command_palette_entries();
    let recent_command_ids = app.command_history.get_recent_commands(100);
    let candidates = App::command_palette_candidates(state, &entries, &recent_command_ids);
    assert!(!candidates.is_empty(), "expected at least one candidate");
    assert_eq!(
        entries[candidates[0].entry_index].action,
        CommandAction::DetachClient
    );
}

#[test]
fn cursor_mode_movement_and_word_navigation() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha beta gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 0 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("w to next word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 6);
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 0 })
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("e to end of word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 9);
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 6 })
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("b to previous word start");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 6);
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 9 })
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('$'), KeyModifiers::NONE))
        .expect("jump to line end");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 15);
    assert!(state.selection_anchor.is_none());

    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE))
        .expect("jump to line start");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("move right");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move left");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 0);
    assert!(state.selection_anchor.is_none());
}

#[test]
fn cursor_mode_word_navigation_crosses_lines_like_gargo() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha".to_string(), "beta gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 0 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("w should cross into next line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 1);
    assert_eq!(state.cursor.col, 0);

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("w should move to next word on second line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 1);
    assert_eq!(state.cursor.col, 5);

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("b should move to previous word on same line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 1);
    assert_eq!(state.cursor.col, 0);

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("b should cross back to previous line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 0);
    assert_eq!(state.cursor.col, 0);
}

#[test]
fn cursor_mode_word_navigation_handles_punctuation_blocks_like_gargo() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["aplio@test z".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 0 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("first w to punctuation block");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 5);

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("second w to next word after punctuation");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 6);

    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("third w to next word after whitespace");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 11);

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("first b back to test");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 6);

    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("second b back to punctuation block");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 5);
}

#[test]
fn cursor_mode_word_end_navigation_crosses_lines_like_gargo() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha".to_string(), "beta gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 4 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("e should jump to end of next word across line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 1);
    assert_eq!(state.cursor.col, 3);

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("e should jump to end of following word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.line, 1);
    assert_eq!(state.cursor.col, 9);
}

#[test]
fn cursor_mode_word_end_navigation_handles_punctuation_blocks_like_gargo() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["aplio@test z".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 0 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("first e should end first word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 4);

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("second e should land on punctuation block");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 5);

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("third e should end second word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 9);

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("fourth e should end final word");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor.col, 11);
}

#[test]
fn cursor_mode_v_toggles_selection_anchor() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha beta gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 4 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .expect("set anchor with v");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 4 })
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .expect("toggle anchor off with v");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert!(state.selection_anchor.is_none());
}

#[test]
fn cursor_mode_space_does_not_toggle_selection_anchor() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha beta gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 4 },
            selection_anchor: Some(super::CursorModePoint { line: 0, col: 1 }),
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .expect("space should not change cursor mode selection");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(state.cursor, super::CursorModePoint { line: 0, col: 4 });
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 1 })
    );
}

#[test]
fn cursor_mode_x_selects_current_line() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha beta".to_string(), "gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 3 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("x should select full line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 0 })
    );
    assert_eq!(state.cursor, super::CursorModePoint { line: 0, col: 9 });
    assert_eq!(App::cursor_mode_selected_text(state), "alpha beta");
}

#[test]
fn cursor_mode_x_extends_line_selection_down() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 0, col: 2 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("first x selects line");
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("second x extends by one line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 0, col: 0 })
    );
    assert_eq!(state.cursor, super::CursorModePoint { line: 1, col: 3 });
    assert_eq!(App::cursor_mode_selected_text(state), "alpha\nbeta");
}

#[test]
fn cursor_mode_x_at_last_line_does_not_move_past_buffer_end() {
    let mut app = build_app_for_resize_test();
    app.view.input_mode = InputMode::CursorMode {
        state: super::CursorModeState {
            pane_id: 1,
            lines: vec!["alpha".to_string(), "beta".to_string()],
            styled_lines: Vec::new(),
            cursor: super::CursorModePoint { line: 1, col: 1 },
            selection_anchor: None,
            viewport_top: 0,
        },
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("first x selects last line");
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("second x should stay on last line");
    let InputMode::CursorMode { state } = &app.view.input_mode else {
        panic!("expected cursor mode");
    };
    assert_eq!(
        state.selection_anchor,
        Some(super::CursorModePoint { line: 1, col: 0 })
    );
    assert_eq!(state.cursor, super::CursorModePoint { line: 1, col: 3 });
    assert_eq!(App::cursor_mode_selected_text(state), "beta");
}

#[test]
fn cursor_mode_selection_extracts_multiline_text() {
    let state = super::CursorModeState {
        pane_id: 1,
        lines: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        styled_lines: Vec::new(),
        cursor: super::CursorModePoint { line: 2, col: 2 },
        selection_anchor: Some(super::CursorModePoint { line: 0, col: 1 }),
        viewport_top: 0,
    };

    assert_eq!(
        App::cursor_mode_selected_text(&state),
        "lpha\nbeta\ngam".to_string()
    );
}

#[test]
fn cursor_mode_enter_exits_to_normal_mode() {
    let mut app = build_app_with_history();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("open cursor mode");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("exit cursor mode");

    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn cursor_mode_copy_for_remote_client_queues_clipboard_ansi() {
    let mut app = build_app_with_history();
    let remote_client_id = 42;
    app.register_client(remote_client_id, 80, 24);

    app.handle_key_event_for_client(
        remote_client_id,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    )
    .expect("remote enter prefix");
    app.handle_key_event_for_client(
        remote_client_id,
        KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
    )
    .expect("remote open copy mode");
    app.handle_key_event_for_client(
        remote_client_id,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    )
    .expect("remote move to beta line");
    app.handle_key_event_for_client(
        remote_client_id,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
    )
    .expect("remote copy beta line");

    assert_eq!(
        app.take_pending_clipboard_ansi_for_client(remote_client_id),
        vec![crate::clipboard::osc52_sequence("beta")]
    );
    assert!(
        app.take_pending_clipboard_ansi_for_client(remote_client_id)
            .is_empty()
    );
}

#[test]
fn cursor_mode_y_keeps_mode_active() {
    let mut app = build_app_with_history();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("open cursor mode");

    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .expect("copy with y");

    assert!(matches!(app.view.input_mode, InputMode::CursorMode { .. }));
}

#[test]
fn tree_left_collapses_window_and_moves_to_parent() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    let rows = app.system_tree_rows(state);
    assert_eq!(state.cursor_row, 1);
    assert!(matches!(rows[1].kind, TreeRowKind::Window { .. }));
    assert!(!rows[1].expanded);

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("left from collapsed window to session");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    assert_eq!(state.cursor_row, 0);
    let rows = app.system_tree_rows(state);
    assert!(matches!(rows[0].kind, TreeRowKind::Session { .. }));
    assert!(rows[0].expanded);

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("collapse session");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    assert_eq!(state.cursor_row, 0);
    let rows = app.system_tree_rows(state);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].kind, TreeRowKind::Session { .. }));
    assert!(!rows[0].expanded);
}

#[test]
fn tree_right_expands_and_descends() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 0;
    }
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("collapse session");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    let rows = app.system_tree_rows(state);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].expanded);

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("expand session");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    let rows = app.system_tree_rows(state);
    assert!(rows[0].expanded);
    assert_eq!(state.cursor_row, 0);

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("move to first child");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected system tree");
    };
    assert_eq!(state.cursor_row, 1);
}

#[test]
fn tree_enter_on_session_switches_active_session() {
    let mut app = build_app_for_resize_test();
    app.create_session();
    app.view.active_session = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    let target_row = match &app.view.input_mode {
        InputMode::SystemTree { state } => {
            let rows = app.system_tree_rows(state);
            rows.iter()
                .position(|row| matches!(row.kind, TreeRowKind::Session { session_index: 1 }))
                .expect("second session row")
        }
        _ => panic!("expected system tree"),
    };
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = target_row;
    }

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("select session");

    assert_eq!(app.view.active_session, 1);
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn tree_enter_on_pane_focuses_pane() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first window");
    let target_pane = app.current_session().window_entries()[1].pane_id;

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);

    let target_row = match &app.view.input_mode {
        InputMode::SystemTree { state } => {
            let rows = app.system_tree_rows(state);
            rows.iter()
                .position(|row| {
                    matches!(
                        row.kind,
                        TreeRowKind::Pane {
                            session_index: 0,
                            pane_id,
                        } if pane_id == target_pane
                    )
                })
                .expect("target pane row")
        }
        _ => panic!("expected system tree"),
    };
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = target_row;
    }

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("select pane");

    assert_eq!(app.current_session().focused_pane_id(), Some(target_pane));
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn tree_r_renames_window_and_returns_to_tree() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 1;
    }

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .expect("start rename");
    assert!(matches!(
        app.view.input_mode,
        InputMode::RenameTreeItem {
            target: RenameTarget::Window { .. },
            ..
        }
    ));

    for ch in "build".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type rename");
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("apply rename");

    let rows = match &app.view.input_mode {
        InputMode::SystemTree { state } => app.system_tree_rows(state),
        _ => panic!("expected tree mode after rename"),
    };
    assert!(rows.iter().any(|row| row.label.contains("window w1:build")));
}

#[test]
fn tree_r_renames_pane_and_returns_to_tree() {
    let mut app = build_app_for_resize_test();
    let target_pane = app
        .current_session()
        .focused_pane_id()
        .expect("focused pane");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    let target_row = match &app.view.input_mode {
        InputMode::SystemTree { state } => {
            let rows = app.system_tree_rows(state);
            rows.iter()
                .position(|row| {
                    matches!(
                        row.kind,
                        TreeRowKind::Pane {
                            session_index: 0,
                            pane_id,
                        } if pane_id == target_pane
                    )
                })
                .expect("target pane row")
        }
        _ => panic!("expected system tree"),
    };
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = target_row;
    }

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .expect("start rename");
    assert!(matches!(
        app.view.input_mode,
        InputMode::RenameTreeItem {
            target: RenameTarget::Pane { .. },
            ..
        }
    ));

    for ch in "logs".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type rename");
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("apply rename");

    let rows = match &app.view.input_mode {
        InputMode::SystemTree { state } => app.system_tree_rows(state),
        _ => panic!("expected tree mode after rename"),
    };
    assert!(rows.iter().any(|row| row.label.contains("pane p1:logs")));
}

#[test]
fn tree_r_rename_escape_keeps_tree_mode() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .expect("start rename");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("cancel rename");
    assert!(matches!(app.view.input_mode, InputMode::SystemTree { .. }));
}

#[test]
fn tree_rename_uses_inline_overlay_and_keeps_status_for_controls() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 1;
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .expect("start rename");

    let overlay = app.system_overlay().expect("tree rename overlay");
    let selected = overlay.selected;
    assert!(
        overlay.candidates[selected].starts_with("rename window: "),
        "selected row should show inline rename prompt"
    );
    assert_eq!(
        overlay.selected_cursor_pos,
        Some(overlay.candidates[selected].chars().count())
    );

    let status = app.status_line();
    assert!(status.contains("tree popup (rename window)"));
    assert!(!status.contains("rename window: "));
}

#[test]
fn tree_rename_overlay_updates_inline_buffer_on_backspace() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 1;
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .expect("start rename");

    for ch in "build".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type rename");
    }
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .expect("backspace rename");

    let overlay = app.system_overlay().expect("tree rename overlay");
    let selected = overlay.selected;
    assert!(overlay.candidates[selected].contains("rename window: buil"));
    assert_eq!(
        overlay.selected_cursor_pos,
        Some(overlay.candidates[selected].chars().count())
    );
}

#[test]
fn prefix_dollar_renames_session_and_returns_normal_mode() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('$'), KeyModifiers::NONE))
        .expect("open rename");
    assert!(matches!(
        app.view.input_mode,
        InputMode::RenameTreeItem {
            target: RenameTarget::Session { .. },
            return_tree: None,
            ..
        }
    ));

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("append rename");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("apply rename");

    assert_eq!(app.current_session().session_name(), "mainx");
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn tree_status_line_mentions_rename_key() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    assert!(app.status_line().contains("r rename"));
}

#[test]
fn tree_preview_shows_real_scrollback_output() {
    let mut app = build_app_with_history();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    let overlay = app.system_overlay().expect("tree overlay");
    assert!(overlay.preview_from_tail);
    assert!(overlay.preview_lines.iter().any(|line| line == "alpha"));
    assert!(overlay.preview_lines.iter().any(|line| line == "beta"));
    assert!(overlay.preview_lines.iter().any(|line| line == "gamma"));
    assert!(
        !overlay
            .preview_lines
            .iter()
            .any(|line| line.starts_with("type:"))
    );
}

#[test]
fn tree_preview_for_session_row_uses_focused_pane_output() {
    let mut app = build_app_with_history();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 0;
    }

    let overlay = app.system_overlay().expect("tree overlay");
    assert!(overlay.preview_from_tail);
    assert!(overlay.preview_lines.iter().any(|line| line == "alpha"));
    assert!(overlay.preview_lines.iter().any(|line| line == "beta"));
    assert!(overlay.preview_lines.iter().any(|line| line == "gamma"));
}

#[test]
fn tree_preview_uses_empty_fallback_when_pane_has_no_output() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    let overlay = app.system_overlay().expect("tree overlay");
    assert!(overlay.preview_from_tail);
    assert_eq!(overlay.preview_lines, vec!["no pane output".to_string()]);
}

#[test]
fn tree_slash_activates_query_mode() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");

    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(state.query_active);
}

#[test]
fn tree_query_filters_by_id_and_name() {
    let mut app = build_app_for_resize_test();
    app.sessions[0].pane_names.insert(1, "logs".to_string());
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
        .expect("type query");
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .expect("type query");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    let rows = app.system_tree_rows(state);
    let candidates = app.system_tree_candidates(state, &rows);
    assert_eq!(candidates.len(), 1);
    assert!(matches!(
        rows[candidates[0].row_index].kind,
        TreeRowKind::Pane {
            session_index: 0,
            pane_id: 1
        }
    ));
}

#[test]
fn osc_title_updates_auto_pane_and_window_names() {
    let mut app = build_app_for_resize_test();
    let window = app.current_session().window_entries()[0].clone();
    let pane_id = window.pane_id;
    let window_id = window.window_id;

    let changed = app.apply_terminal_events_for_session(
        0,
        vec![PaneTerminalEvent {
            pane_id,
            event: TerminalEvent::TitleChanged {
                title: Some("build".to_string()),
            },
        }],
    );
    assert!(changed);
    assert_eq!(app.effective_pane_name(0, pane_id), Some("build"));
    assert_eq!(app.effective_window_name(0, window_id), Some("build"));
}

#[test]
fn render_snapshot_window_title_uses_focused_osc_title() {
    let mut app = build_app_for_resize_test();
    let pane_id = app.current_session().window_entries()[0].pane_id;
    app.apply_terminal_events_for_session(
        0,
        vec![PaneTerminalEvent {
            pane_id,
            event: TerminalEvent::TitleChanged {
                title: Some("build".to_string()),
            },
        }],
    );
    app.request_render(false);

    let snapshot = app
        .render_snapshot_for_client(super::LOCAL_CLIENT_ID)
        .expect("render snapshot");
    assert_eq!(snapshot.window_title.as_deref(), Some("build"));
    app.finish_render_cycle();
}

#[test]
fn render_snapshot_window_title_falls_back_to_osc7_cwd_when_title_cleared() {
    let mut app = build_app_for_resize_test();
    let pane_id = app.current_session().window_entries()[0].pane_id;
    app.apply_terminal_events_for_session(
        0,
        vec![
            PaneTerminalEvent {
                pane_id,
                event: TerminalEvent::TitleChanged {
                    title: Some("build".to_string()),
                },
            },
            PaneTerminalEvent {
                pane_id,
                event: TerminalEvent::CwdChanged {
                    cwd: "/tmp/work".to_string(),
                },
            },
            PaneTerminalEvent {
                pane_id,
                event: TerminalEvent::TitleChanged { title: None },
            },
        ],
    );
    app.request_render(false);

    let snapshot = app
        .render_snapshot_for_client(super::LOCAL_CLIENT_ID)
        .expect("render snapshot");
    assert_eq!(snapshot.window_title.as_deref(), Some("/tmp/work"));
    app.finish_render_cycle();
}

#[test]
fn render_snapshot_window_title_is_none_without_terminal_metadata() {
    let mut app = build_app_for_resize_test();
    app.request_render(false);

    let snapshot = app
        .render_snapshot_for_client(super::LOCAL_CLIENT_ID)
        .expect("render snapshot");
    assert_eq!(snapshot.window_title, None);
    app.finish_render_cycle();
}

#[test]
fn render_snapshot_window_title_is_isolated_per_client_focus() {
    let mut app = build_app_with_named_sessions_for_attach();
    let alpha_pane = app.sessions[0]
        .session
        .focused_pane_id()
        .expect("alpha focused pane");
    let beta_pane = app.sessions[1]
        .session
        .focused_pane_id()
        .expect("beta focused pane");
    app.apply_terminal_events_for_session(
        0,
        vec![PaneTerminalEvent {
            pane_id: alpha_pane,
            event: TerminalEvent::TitleChanged {
                title: Some("alpha-title".to_string()),
            },
        }],
    );
    app.apply_terminal_events_for_session(
        1,
        vec![PaneTerminalEvent {
            pane_id: beta_pane,
            event: TerminalEvent::CwdChanged {
                cwd: "/tmp/beta".to_string(),
            },
        }],
    );

    app.register_client(1, 80, 24);
    app.register_client(2, 80, 24);
    let alpha_target = AttachTarget::parse("alpha-1").expect("parse alpha target");
    let beta_target = AttachTarget::parse("beta-2").expect("parse beta target");
    app.apply_attach_target_for_client(1, &alpha_target)
        .expect("attach client 1 to alpha");
    app.apply_attach_target_for_client(2, &beta_target)
        .expect("attach client 2 to beta");
    app.request_render(false);

    let client_one = app
        .render_snapshot_for_client(1)
        .expect("snapshot for client 1");
    let client_two = app
        .render_snapshot_for_client(2)
        .expect("snapshot for client 2");

    assert_eq!(client_one.window_title.as_deref(), Some("alpha-title"));
    assert_eq!(client_two.window_title.as_deref(), Some("/tmp/beta"));
    app.finish_render_cycle();
}

#[test]
fn manual_window_name_overrides_osc_window_name() {
    let mut app = build_app_for_resize_test();
    let window = app.current_session().window_entries()[0].clone();
    let pane_id = window.pane_id;
    let window_id = window.window_id;
    app.sessions[0]
        .window_names
        .insert(window_id, "manual".to_string());

    let changed = app.apply_terminal_events_for_session(
        0,
        vec![PaneTerminalEvent {
            pane_id,
            event: TerminalEvent::TitleChanged {
                title: Some("from-osc".to_string()),
            },
        }],
    );
    assert!(changed);
    assert_eq!(app.effective_window_name(0, window_id), Some("manual"));
    assert_eq!(
        app.sessions[0]
            .window_auto_names
            .get(&window_id)
            .map(String::as_str),
        Some("from-osc")
    );
}

#[test]
fn manual_pane_name_overrides_osc_pane_name() {
    let mut app = build_app_for_resize_test();
    let pane_id = app.current_session().window_entries()[0].pane_id;
    app.sessions[0]
        .pane_names
        .insert(pane_id, "logs".to_string());

    let changed = app.apply_terminal_events_for_session(
        0,
        vec![PaneTerminalEvent {
            pane_id,
            event: TerminalEvent::TitleChanged {
                title: Some("from-osc".to_string()),
            },
        }],
    );
    assert!(changed);
    assert_eq!(app.effective_pane_name(0, pane_id), Some("logs"));
    assert_eq!(
        app.sessions[0]
            .pane_auto_names
            .get(&pane_id)
            .map(String::as_str),
        Some("from-osc")
    );
}

#[test]
fn tree_query_left_right_and_ctrl_f_b_a_e_moves_cursor() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    for ch in "abcd".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type query");
    }

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("left");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("left");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("right");
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
        .expect("ctrl+b");
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        .expect("ctrl+f");
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("ctrl+a");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .expect("ctrl+e");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(state.query_active);
    assert_eq!(state.query_input.text, "abcd");
    assert_eq!(
        state.query_input.cursor,
        state.query_input.text.chars().count()
    );
}

#[test]
fn tree_query_ctrl_left_right_and_ctrl_w_k_u_edit_query() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    for ch in "alpha beta gamma".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type query");
    }

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("ctrl+a");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
        .expect("ctrl+right");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
        .expect("ctrl+right");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.cursor, 11);

    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
        .expect("ctrl+left");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
        .expect("ctrl+right to gamma");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
        .expect("ctrl+w");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.text, "alpha gamma");
    assert_eq!(state.query_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
        .expect("ctrl+k");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.text, "alpha ");
    assert_eq!(state.query_input.cursor, 6);

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .expect("ctrl+u");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.text, "");
    assert_eq!(state.query_input.cursor, 0);
    assert!(state.query_active);
}

#[test]
fn tree_query_backspace_deletes_previous_char() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .expect("type a");
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
        .expect("type b");

    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .expect("backspace");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.query_input.text, "a");
    assert_eq!(state.query_input.cursor, 1);
}

#[test]
fn tree_down_from_query_focus_selects_first_candidate_and_leaves_query_focus() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("type query");

    let first = match &app.view.input_mode {
        InputMode::SystemTree { state } => {
            let rows = app.system_tree_rows(state);
            let candidates = app.system_tree_candidates(state, &rows);
            candidates.first().expect("first candidate").row_index
        }
        _ => panic!("expected tree mode"),
    };

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("down to candidates");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);
    assert_eq!(state.cursor_row, first);
}

#[test]
fn tree_up_on_first_candidate_returns_to_query_focus() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("type query");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("down to candidates");

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .expect("up to query");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(state.query_active);
}

#[test]
fn tree_candidate_left_right_still_collapse_expand_after_query_focus() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = 0;
    }
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("collapse session");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    let rows = app.system_tree_rows(state);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].expanded);

    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("enter candidate focus");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .expect("expand session");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    let rows = app.system_tree_rows(state);
    assert!(!state.query_active);
    assert!(rows[0].expanded);
}

#[test]
fn tree_query_ctrl_n_p_j_switches_to_candidates_and_navigates() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first pane");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("type query");

    let (first, second) = match &app.view.input_mode {
        InputMode::SystemTree { state } => {
            let rows = app.system_tree_rows(state);
            let candidates = app.system_tree_candidates(state, &rows);
            assert!(
                candidates.len() >= 2,
                "expected at least two pane candidates"
            );
            (candidates[0].row_index, candidates[1].row_index)
        }
        _ => panic!("expected tree mode"),
    };

    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = first;
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .expect("ctrl+n");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);
    assert_eq!(state.cursor_row, second);

    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("re-enter query");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = first;
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("ctrl+j");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);
    assert_eq!(state.cursor_row, second);

    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("re-enter query");
    if let InputMode::SystemTree { state } = &mut app.view.input_mode {
        state.cursor_row = first;
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .expect("ctrl+p");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);
    assert_eq!(state.cursor_row, first);
}

#[test]
fn tree_query_navigation_shortcuts_with_no_candidates_keep_query_focus() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    for ch in ['z', 'z', 'z'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type query");
    }

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("down with empty candidates");
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .expect("ctrl+n with empty candidates");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("ctrl+j with empty candidates");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .expect("ctrl+p with empty candidates");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(state.query_active);
    assert_eq!(state.query_input.text, "zzz");
}

#[test]
fn tree_down_moves_within_filtered_candidates() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    app.current_session_mut()
        .focus_window_number(1)
        .expect("focus first pane");

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    expand_all_system_tree_windows(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .expect("type query");

    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    let rows = app.system_tree_rows(state);
    let candidates = app.system_tree_candidates(state, &rows);
    assert_eq!(candidates.len(), 2);
    let first = candidates[0].row_index;
    let second = candidates[1].row_index;
    assert_eq!(state.cursor_row, first);
    assert!(state.query_active);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("down enters candidate focus");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);
    assert_eq!(state.cursor_row, first);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("move down");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert_eq!(state.cursor_row, second);
}

#[test]
fn tree_enter_with_no_candidates_keeps_tree_mode() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");
    for ch in ['z', 'z', 'z'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .expect("type query");
    }

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect("press enter");
    assert!(matches!(app.view.input_mode, InputMode::SystemTree { .. }));
}

#[test]
fn tree_escape_closes_query_before_closing_tree() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
        .expect("activate query");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("escape query");
    let InputMode::SystemTree { state } = &app.view.input_mode else {
        panic!("expected tree mode");
    };
    assert!(!state.query_active);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("close tree");
    assert!(matches!(app.view.input_mode, InputMode::Normal));
}

#[test]
fn prefix_d_returns_detach_signal() {
    let mut app = build_app_for_resize_test();

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    assert_eq!(signal, AppSignal::None);
    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .expect("detach");
    assert_eq!(signal, AppSignal::DetachClient);
}

#[test]
fn detach_does_not_trigger_while_renaming() {
    let mut app = build_app_for_resize_test();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('$'), KeyModifiers::NONE))
        .expect("open session rename");
    assert!(matches!(
        app.view.input_mode,
        InputMode::RenameTreeItem { .. }
    ));

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .expect("type d in rename");
    assert_eq!(signal, AppSignal::None);
    assert!(matches!(
        app.view.input_mode,
        InputMode::RenameTreeItem { .. }
    ));
}

#[test]
fn prefix_q_still_quits_without_detach_signal() {
    let mut app = build_app_for_resize_test();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("quit");

    assert_eq!(signal, AppSignal::None);
    assert!(app.should_quit);
}

#[test]
fn enter_leave_lock_mode_via_action() {
    let mut app = build_app_for_resize_test();

    app.handle_action(CommandAction::EnterLockMode);
    assert!(app.view.locked_input);
    assert!(app.status_line().contains("LOCK"));

    app.handle_action(CommandAction::LeaveLockMode);
    assert!(!app.view.locked_input);
    assert!(!app.status_line().contains("LOCK"));
}

#[test]
fn lock_mode_forwards_keys_and_blocks_prefix_commands() {
    let (mut app, writes) = build_recording_app_one_session();

    app.handle_action(CommandAction::EnterLockMode);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("send ctrl+j");
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("send q");

    assert!(!app.should_quit);
    assert_eq!(
        take_recorded_writes(&writes),
        vec![(1, vec![0x0a]), (1, vec![b'q'])]
    );

    app.handle_action(CommandAction::LeaveLockMode);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("quit");
    assert!(app.should_quit);
}

#[test]
fn lock_mode_escape_exits_without_forwarding_first_escape() {
    let (mut app, writes) = build_recording_app_one_session();

    app.handle_action(CommandAction::EnterLockMode);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("escape exits lock mode");

    assert!(!app.view.locked_input);
    assert!(take_recorded_writes(&writes).is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .expect("second escape forwards after unlock");
    assert_eq!(take_recorded_writes(&writes), vec![(1, vec![0x1b])]);
}

#[test]
fn normal_send_bytes_does_not_force_render_when_view_is_following() {
    let (mut app, writes) = build_recording_app_one_session();
    app.needs_render = false;

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("send plain key");

    assert_eq!(signal, AppSignal::None);
    assert!(
        !app.needs_render,
        "plain pane input should not force render"
    );
    assert_eq!(take_recorded_writes(&writes), vec![(1, vec![b'x'])]);
}

#[test]
fn normal_send_bytes_marks_render_when_manual_scroll_is_reset() {
    let (mut app, writes) = build_recording_app_with_history();
    let pane_view_rows = usize::from(app.view.rows.saturating_sub(1)).max(1);
    app.current_session_mut()
        .scroll_focused_pane(8, pane_view_rows);
    app.needs_render = false;

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("send plain key");

    assert_eq!(signal, AppSignal::None);
    assert!(
        app.needs_render,
        "resetting manual scroll should force render"
    );
    assert_eq!(take_recorded_writes(&writes), vec![(1, vec![b'x'])]);
}

#[test]
fn entering_prefix_marks_render_without_forwarding_input() {
    let (mut app, writes) = build_recording_app_one_session();
    app.needs_render = false;

    let signal = app
        .handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");

    assert_eq!(signal, AppSignal::None);
    assert!(
        app.needs_render,
        "prefix toggle should update status render"
    );
    assert!(take_recorded_writes(&writes).is_empty());
}

#[test]
fn synchronize_panes_fans_out_keys_and_paste_to_active_window() {
    let (mut app, writes) = build_recording_app_one_session();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split focused pane");

    assert!(!app.status_line().contains("SYNC"));
    let signal = app.handle_action(CommandAction::ToggleSynchronizePanes);
    assert_eq!(signal, AppSignal::None);
    assert!(app.status_line().contains("SYNC"));

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("send key with sync");
    app.handle_paste_text("clip".to_string())
        .expect("paste with sync");

    assert_eq!(
        take_recorded_writes(&writes),
        vec![
            (1, vec![b'x']),
            (2, vec![b'x']),
            (1, b"clip".to_vec()),
            (2, b"clip".to_vec())
        ]
    );
}

#[test]
fn zoom_and_sync_status_indicators_toggle() {
    let mut app = build_app_for_resize_test();
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split focused pane");

    assert!(!app.status_line().contains("ZOOM"));
    assert!(!app.status_line().contains("SYNC"));

    let _ = app.handle_action(CommandAction::ToggleZoom);
    assert!(app.status_line().contains("ZOOM"));

    let _ = app.handle_action(CommandAction::ToggleSynchronizePanes);
    let status = app.status_line();
    assert!(status.contains("ZOOM"));
    assert!(status.contains("SYNC"));

    let _ = app.handle_action(CommandAction::ToggleZoom);
    let status = app.status_line();
    assert!(!status.contains("ZOOM"));
    assert!(status.contains("SYNC"));
}

#[test]
fn mouse_click_focuses_pane_when_mouse_mode_enabled() {
    let mut app = build_app_for_resize_test();
    app.mouse_enabled = true;
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    let frame = app.current_session().frame(app.view.cols, app.view.rows);
    let left = frame
        .panes
        .iter()
        .min_by_key(|pane| pane.rect.x)
        .expect("left pane");
    let right = frame
        .panes
        .iter()
        .max_by_key(|pane| pane.rect.x)
        .expect("right pane");

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        left.rect.x as u16,
        left.rect.y as u16,
    ))
    .expect("click left pane");
    assert_eq!(app.current_session().focused_pane_id(), Some(left.pane_id));

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        right.rect.x as u16,
        right.rect.y as u16,
    ))
    .expect("click right pane");
    assert_eq!(app.current_session().focused_pane_id(), Some(right.pane_id));
}

#[test]
fn mouse_drag_on_divider_resizes_adjacent_panes() {
    let mut app = build_app_for_resize_test();
    app.mouse_enabled = true;
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    let before = app
        .current_session()
        .layout_snapshot(app.view.cols, app.view.rows);
    let frame = app.current_session().frame(app.view.cols, app.view.rows);
    let divider = frame
        .dividers
        .iter()
        .find(|divider| {
            divider.orientation == crate::ui::window_manager::DividerOrientation::Vertical
        })
        .copied()
        .expect("vertical divider");
    let left_pane = frame
        .panes
        .iter()
        .find(|pane| pane.rect.x + pane.rect.width == divider.x)
        .expect("left pane")
        .pane_id;
    let left_before = before.windows[0]
        .panes
        .iter()
        .find(|pane| pane.pane_id == left_pane)
        .expect("left pane before")
        .rect
        .width;

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        divider.x as u16,
        divider.y as u16,
    ))
    .expect("mouse down on divider");
    app.handle_mouse_event(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        divider.x.saturating_add(3) as u16,
        divider.y as u16,
    ))
    .expect("mouse drag divider");
    app.handle_mouse_event(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        divider.x.saturating_add(3) as u16,
        divider.y as u16,
    ))
    .expect("mouse up divider");

    let after = app
        .current_session()
        .layout_snapshot(app.view.cols, app.view.rows);
    let left_after = after.windows[0]
        .panes
        .iter()
        .find(|pane| pane.pane_id == left_pane)
        .expect("left pane after")
        .rect
        .width;
    assert_ne!(left_after, left_before);
}

#[test]
fn mouse_divider_hit_testing_uses_sidebar_shifted_geometry() {
    let mut app = build_app_for_resize_test();
    app.mouse_enabled = true;
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .expect("toggle side window tree");
    assert!(app.side_window_tree_is_open());

    let snapshot = app.take_render_snapshot().expect("render snapshot");
    let divider = snapshot
        .frame
        .dividers
        .iter()
        .find(|divider| {
            divider.orientation == crate::ui::window_manager::DividerOrientation::Vertical
        })
        .copied()
        .expect("vertical divider");

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        divider.x as u16,
        divider.y as u16,
    ))
    .expect("mouse down on shifted divider");

    assert!(
        app.view.mouse_drag.is_some(),
        "expected divider drag state when clicking rendered divider position"
    );
}

#[test]
fn mouse_is_ignored_in_lock_mode_and_non_normal_modes() {
    let mut app = build_app_for_resize_test();
    app.mouse_enabled = true;
    app.current_session_mut()
        .split_focused(crate::ui::window_manager::SplitAxis::Vertical, 80, 24)
        .expect("split pane");
    let frame = app.current_session().frame(app.view.cols, app.view.rows);
    let left = frame
        .panes
        .iter()
        .min_by_key(|pane| pane.rect.x)
        .expect("left pane");
    let right = frame
        .panes
        .iter()
        .max_by_key(|pane| pane.rect.x)
        .expect("right pane");
    app.current_session_mut()
        .focus_pane_id(left.pane_id)
        .expect("focus left pane");

    app.handle_action(CommandAction::EnterLockMode);
    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        right.rect.x as u16,
        right.rect.y as u16,
    ))
    .expect("click while locked");
    assert_eq!(app.current_session().focused_pane_id(), Some(left.pane_id));

    app.handle_action(CommandAction::LeaveLockMode);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .expect("open tree mode");
    assert!(matches!(app.view.input_mode, InputMode::SystemTree { .. }));
    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        right.rect.x as u16,
        right.rect.y as u16,
    ))
    .expect("click in tree mode");
    assert!(matches!(app.view.input_mode, InputMode::SystemTree { .. }));
    assert_eq!(app.current_session().focused_pane_id(), Some(left.pane_id));
}

#[test]
fn mouse_selection_anchor_remains_fixed_while_scrolling_and_dragging() {
    let (mut app, _writes) = build_recording_app_with_history();
    app.mouse_enabled = true;

    let frame = app.current_session().frame(app.view.cols, app.view.rows);
    let pane = frame.panes.first().expect("pane frame");
    let down_col = pane.rect.x as u16;
    let down_row = (pane.rect.y + 5) as u16;
    let expected_anchor_abs_row = pane.view_row_origin + 5;

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        down_col,
        down_row,
    ))
    .expect("start selection");
    let started = app.view.text_selection.expect("selection state after down");
    assert_eq!(started.start_abs_row, expected_anchor_abs_row);

    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollUp, down_col, down_row))
        .expect("scroll up once");
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollUp, down_col, down_row))
        .expect("scroll up twice");

    app.handle_mouse_event(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        down_col,
        (pane.rect.y + 1) as u16,
    ))
    .expect("continue dragging after scroll");

    let updated = app.view.text_selection.expect("selection state after drag");
    assert_eq!(updated.start_abs_row, expected_anchor_abs_row);
    assert!(
        updated.end_abs_row < expected_anchor_abs_row,
        "expected drag after scroll-up to move selection end above original anchor"
    );
}

#[test]
fn scrolled_view_keeps_ansi_colors_from_scrollback() {
    let (mut app, _writes) = build_recording_app_with_output(colored_history_output_chunks());
    assert!(
        app.current_session_mut().poll_output(),
        "expected pane output"
    );
    app.mouse_enabled = true;

    let before = app.current_session().frame(app.view.cols, app.view.rows);
    let follow_origin = before
        .panes
        .first()
        .expect("pane before scroll")
        .view_row_origin;

    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollUp, 0, 0))
        .expect("scroll up");
    let scrolled = app.current_session().frame(app.view.cols, app.view.rows);
    let pane = scrolled.panes.first().expect("pane after scroll");
    assert!(
        pane.view_row_origin < follow_origin,
        "expected scroll-up to move viewport toward scrollback"
    );

    let top_row = pane.rows.first().expect("top row");
    let first_text_cell = top_row
        .iter()
        .find(|cell| cell.ch != ' ' && cell.ch != '\0')
        .expect("expected text in top row");
    assert_eq!(first_text_cell.ch, 'r');
    assert_eq!(first_text_cell.style.fg, Some(Color::AnsiValue(1)));
}

#[test]
fn cursor_mode_frame_preserves_ansi_colors() {
    let (mut app, _writes) =
        build_recording_app_with_output(vec![b"\x1b[31mA\x1b[0m\r\nB\r\n".to_vec()]);
    assert!(
        app.current_session_mut().poll_output(),
        "expected pane output"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
        .expect("open cursor mode");

    let snapshot = app
        .render_snapshot_for_client(super::LOCAL_CLIENT_ID)
        .expect("cursor mode snapshot");
    let pane = snapshot
        .frame
        .panes
        .iter()
        .find(|pane| pane.focused)
        .expect("focused pane");
    let cell = pane
        .rows
        .iter()
        .flatten()
        .find(|cell| cell.ch == 'A')
        .expect("colored cell in cursor mode");
    assert_eq!(cell.style.fg, Some(Color::AnsiValue(1)));
}

#[test]
fn key_press_after_mouse_scroll_returns_to_follow_mode_and_executes_input() {
    let (mut app, writes) = build_recording_app_with_history();
    app.mouse_enabled = true;

    let initial = app.current_session().frame(app.view.cols, app.view.rows);
    assert!(
        initial.focused_cursor.is_some(),
        "expected cursor visible in follow mode before scrolling"
    );

    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollUp, 0, 0))
        .expect("scroll up");
    let scrolled = app.current_session().frame(app.view.cols, app.view.rows);
    assert!(
        scrolled.focused_cursor.is_none(),
        "expected cursor hidden when viewport is scrolled away from follow mode"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("send key");

    let writes = take_recorded_writes(&writes);
    assert!(
        writes.iter().any(|(_, bytes)| bytes == b"x"),
        "expected typed key to still be forwarded to pane; writes={writes:?}"
    );

    let after = app.current_session().frame(app.view.cols, app.view.rows);
    assert!(
        after.focused_cursor.is_some(),
        "expected key press to restore follow mode cursor visibility"
    );
}

#[test]
fn prefix_key_after_mouse_scroll_returns_to_follow_mode_and_enters_prefix_mode() {
    let (mut app, _writes) = build_recording_app_with_history();
    app.mouse_enabled = true;

    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollUp, 0, 0))
        .expect("scroll up");
    let scrolled = app.current_session().frame(app.view.cols, app.view.rows);
    assert!(
        scrolled.focused_cursor.is_none(),
        "expected cursor hidden when viewport is scrolled away from follow mode"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");

    assert!(
        app.view.keys.prefix_active(),
        "expected prefix key to still execute after scroll reset"
    );
    let after = app.current_session().frame(app.view.cols, app.view.rows);
    assert!(
        after.focused_cursor.is_some(),
        "expected key press to restore follow mode cursor visibility"
    );
}

#[test]
fn ctrl_d_on_single_pane_sets_quit() {
    let mut app = build_app_with_write_behavior(WriteBehavior::Eio);

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");

    assert!(app.should_quit);
}

#[test]
fn prefix_c_creates_new_window() {
    let mut app = build_app_for_resize_test();
    let before = app.current_session().pane_count();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .expect("enter prefix");
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .expect("create window");

    assert_eq!(app.current_session().pane_count(), before + 1);
}

#[test]
fn ctrl_d_on_multi_pane_closes_focused_pane() {
    let mut app = build_app_with_write_behavior(WriteBehavior::Eio);
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    let before = app.current_session().pane_count();

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");

    assert!(!app.should_quit);
    assert_eq!(app.current_session().pane_count(), before - 1);
}

#[test]
fn ctrl_d_closed_process_multi_pane_closes_on_tick() {
    let mut app = build_app_with_close_on_write_behavior(CloseOnWriteBehavior::CTRL_D);
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    let before = app.current_session().pane_count();

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");
    app.tick();

    assert!(!app.should_quit);
    assert_eq!(app.current_session().pane_count(), before - 1);
}

#[test]
fn ctrl_d_closed_process_single_pane_quits_on_tick() {
    let mut app = build_app_with_close_on_write_behavior(CloseOnWriteBehavior::CTRL_D);

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");
    app.tick();

    assert!(app.should_quit);
}

#[test]
fn ctrl_d_closed_process_single_pane_switches_to_existing_session_on_tick() {
    let mut app = build_app_with_close_on_write_behavior(CloseOnWriteBehavior::CTRL_D);
    add_fake_session(&mut app, "backup", "backup-2");
    let closed_session_id = app.sessions[0].session_id.clone();

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");
    app.tick();

    assert!(!app.should_quit);
    assert_eq!(app.sessions.len(), 1);
    assert_ne!(app.sessions[0].session_id, closed_session_id);
    assert_eq!(app.view.active_session, 0);
}

#[test]
fn ctrl_d_non_close_error_keeps_session_open() {
    let mut app = build_app_with_write_behavior(WriteBehavior::PermissionDenied);
    let before = app.current_session().pane_count();

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .expect("handle ctrl+d");

    assert!(!app.should_quit);
    assert_eq!(app.current_session().pane_count(), before);
    assert!(app.view.status_message.is_some());
}

#[test]
fn normal_key_on_closed_single_pane_sets_quit() {
    let mut app = build_app_with_write_behavior(WriteBehavior::Eio);

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .expect("handle normal key on closed pane");

    assert!(app.should_quit);
}

#[test]
fn normal_key_on_closed_single_pane_switches_to_existing_session() {
    let mut app = build_app_with_write_behavior(WriteBehavior::Eio);
    add_fake_session(&mut app, "backup", "backup-2");
    let closed_session_id = app.sessions[0].session_id.clone();

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .expect("handle normal key on closed pane");

    assert!(!app.should_quit);
    assert_eq!(app.sessions.len(), 1);
    assert_ne!(app.sessions[0].session_id, closed_session_id);
    assert_eq!(app.view.active_session, 0);
}

#[test]
fn normal_key_on_closed_multi_pane_closes_focused_pane() {
    let mut app = build_app_with_write_behavior(WriteBehavior::Eio);
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    let before = app.current_session().pane_count();

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .expect("handle normal key on closed pane");

    assert!(!app.should_quit);
    assert_eq!(app.current_session().pane_count(), before - 1);
}

#[test]
fn exit_command_closed_process_multi_pane_closes_on_tick() {
    let mut app = build_app_with_close_on_write_behavior(CloseOnWriteBehavior::EXIT_COMMAND);
    app.current_session_mut()
        .new_window(80, 24)
        .expect("create second pane");
    let before = app.current_session().pane_count();

    app.handle_paste("exit\r".to_string())
        .expect("send exit command");
    app.tick();

    assert!(!app.should_quit);
    assert_eq!(app.current_session().pane_count(), before - 1);
}

#[test]
fn closed_error_classifier_matches_eio_and_broken_pipe() {
    assert!(is_closed_pane_error(&io::Error::from_raw_os_error(5)));
    assert!(is_closed_pane_error(&io::Error::new(
        io::ErrorKind::BrokenPipe,
        "broken"
    )));
    assert!(!is_closed_pane_error(&io::Error::new(
        io::ErrorKind::PermissionDenied,
        "denied"
    )));
}
