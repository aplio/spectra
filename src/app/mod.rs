mod actions;
mod agents;
mod api_support;
mod clients;
mod command_dispatch;
mod command_palette;
mod copy_mode;
#[cfg(unix)]
pub mod handoff;
mod hooks;
mod ime;
mod input;
mod keybindings;
mod open_click;
mod persistence;
mod plugins;
mod render_snapshot;
mod system_tree;
#[cfg(test)]
mod tests;
mod types;

use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use crossterm::style::Color;

use crate::attach_target::AttachTarget;
use crate::cli::Cli;
use crate::command_history::CommandHistory;
use crate::config;
use crate::input::{
    CommandAction, InputAction, KITTY_FLAG_DISAMBIGUATE, KITTY_FLAG_REPORT_ALL, KeyMapper,
    encode_key_to_bytes, encode_key_to_bytes_kitty,
};
use crate::io::host_colors::HostColors;
use crate::runtime::event_loop::{FRAME_DURATION_60_FPS, poll_event_for};
use crate::session::manager::SessionOptions;
use crate::session::manager::{PaneTerminalEvent, SessionManager};
use crate::session::terminal_state::{CellStyle, TerminalEvent};
use crate::storage::{DataStore, unix_time_now};
use crate::ui::window_manager::{Direction, WindowId};
use types::*;

pub type ClientId = u64;
pub const LOCAL_CLIENT_ID: ClientId = 0;

/// BEL byte sent to a client's host terminal over the passthrough channel
/// when a guest command finishes (`[command_finish]`).
const BELL: &str = "\x07";

fn parse_hex_color(value: &str) -> Option<Color> {
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if hex.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb { r, g, b })
}

fn default_status_style() -> CellStyle {
    CellStyle {
        fg: Some(DEFAULT_STATUS_FG),
        bg: Some(DEFAULT_STATUS_BG),
        ..CellStyle::default()
    }
}

fn status_style_from_config(status: &config::StatusConfig) -> CellStyle {
    let mut style = default_status_style();
    if let Some(background) = status.background.as_deref().and_then(parse_hex_color) {
        style.bg = Some(background);
    }
    if let Some(foreground) = status.foreground.as_deref().and_then(parse_hex_color) {
        style.fg = Some(foreground);
    }
    style
}

fn normalize_editor_command(editor: Option<String>) -> Option<String> {
    editor.and_then(|editor| {
        let trimmed = editor.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSignal {
    None,
    DetachClient,
}

/// Construction inputs shared by every `App` build path (fresh session,
/// disk restore, live handoff).
struct AppBootstrap {
    options: SessionOptions,
    store: DataStore,
    runtime_ui: RuntimeUiConfig,
    started_unix: u64,
}

pub struct RenderSnapshot {
    pub frame: crate::session::manager::RenderFrame,
    pub status_line: String,
    pub status_style: CellStyle,
    pub window_title: Option<String>,
    pub overlay: Option<crate::ui::render::SystemOverlay>,
    pub side_window_tree: Option<crate::ui::render::SideWindowTree>,
    pub cols: u16,
    pub rows: u16,
    pub full_clear: bool,
    /// Whether the client's host terminal should have mouse capture enabled
    /// for this frame: spectra's own mouse handling is on, or a guest in the
    /// viewed window requested mouse reporting. Otherwise the host stays
    /// uncaptured so native terminal mouse features (text selection, link
    /// clicks) keep working.
    pub wants_mouse_capture: bool,
}

pub struct App {
    sessions: Vec<ManagedSession>,
    view: ClientViewState,
    next_session_ordinal: usize,
    session_template: SessionOptions,
    key_template: KeyMapper,
    status_format: String,
    status_style: CellStyle,
    sidebar_formats: SidebarFormats,
    hooks: config::HooksConfig,
    editor_command: Option<String>,
    agent_notify: config::AgentNotifyMode,
    command_finish: config::CommandFinishConfig,
    ime: config::ImeConfig,
    /// Whether the `[ime]` prefix input-source switch currently considers
    /// the prefix pending (the ascii command has run, restore has not).
    prefix_input_source_switched: bool,
    editor_pane_close_targets: Vec<EditorPaneCloseTarget>,
    store: DataStore,
    command_history: CommandHistory,
    started_unix: u64,
    mouse_enabled: bool,
    open_click: config::OpenClickModifier,
    client_focus_profiles: HashMap<String, PersistedClientFocusState>,
    client_identities: HashMap<ClientId, String>,
    active_client_id: ClientId,
    inactive_client_states: HashMap<ClientId, ClientViewState>,
    should_quit: bool,
    /// When `Some`, a `prefix q` quit confirmation is armed until this
    /// instant; a second `prefix q` within the window quits.
    quit_confirm_deadline: Option<Instant>,
    needs_render: bool,
    needs_full_clear: bool,
    /// API events awaiting fan-out to subscribed API connections
    /// (bounded by [`API_EVENT_QUEUE_MAX`]; drained by the server loop).
    pending_api_events: Vec<crate::api::ApiEvent>,
    /// Plugin discovery/dispatch/service supervision. Inactive (a no-op)
    /// until the server calls [`App::load_plugins`].
    plugins: crate::plugin::PluginHost,
    /// Runtime agent-detection registry: built-in manifests plus
    /// plugin-provided ones, swapped wholesale on plugin (re)load.
    agent_manifests: std::sync::Arc<Vec<crate::agent::AgentManifest>>,
    available_update: Option<String>,
    renderer: crate::ui::render::FrameRenderer,
}

impl App {
    pub fn new(cli: Cli) -> io::Result<Self> {
        let (cols, rows) = crossterm::terminal::size()?;
        Self::new_with_size(cli, cols, rows)
    }

    pub fn new_with_size(cli: Cli, cols: u16, rows: u16) -> io::Result<Self> {
        let AppBootstrap {
            mut options,
            store,
            runtime_ui,
            started_unix,
        } = Self::bootstrap_from_cli(cli)?;
        let command_history = CommandHistory::new_with_data_dir(store.base_dir().to_path_buf());

        if let Some(mut restored) = Self::restore_from_runtime_state(
            &store,
            started_unix,
            options.clone(),
            runtime_ui.clone(),
            cols,
            rows,
        )? {
            restored.persist_active_session_info();
            restored.write_log("session restored");
            return Ok(restored);
        }

        let first_ordinal = 1;
        options.session_id = session_id_for(&options.session_name, first_ordinal);
        let mut session = SessionManager::new(options.clone(), cols, rows)?;
        session.resize(cols, rows)?;

        let first_session_id = session_id_for(session.session_name(), first_ordinal);

        let sessions = vec![ManagedSession {
            ordinal: first_ordinal,
            session_id: first_session_id,
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
            agents: AgentTracking::default(),
        }];
        let pane_histories_by_session = default_pane_histories_for_managed_sessions(&sessions);
        let client_identities =
            HashMap::from([(LOCAL_CLIENT_ID, LOCAL_CLIENT_FOCUS_IDENTITY.to_string())]);

        let mut app = Self {
            sessions,
            view: ClientViewState {
                keys: runtime_ui.keys.clone(),
                input_mode: InputMode::Normal,
                status_message: None,
                locked_input: false,
                mouse_drag: None,
                text_selection: None,
                selection_autoscroll: None,
                click_chain: None,
                pending_clipboard_ansi: Vec::new(),
                pending_passthrough_ansi: Vec::new(),
                pending_image_paste_request: false,
                cols,
                rows,
                active_session: 0,
                pane_histories_by_session,
                side_window_tree_open: runtime_ui.sidebar_default_open,
                search_history: Vec::new(),
            },
            next_session_ordinal: 2,
            session_template: options,
            key_template: runtime_ui.keys,
            status_format: runtime_ui.status_format,
            status_style: runtime_ui.status_style,
            sidebar_formats: runtime_ui.sidebar_formats,
            hooks: runtime_ui.hooks,
            editor_command: runtime_ui.editor_command,
            agent_notify: runtime_ui.agent_notify,
            command_finish: runtime_ui.command_finish,
            ime: runtime_ui.ime,
            prefix_input_source_switched: false,
            editor_pane_close_targets: Vec::new(),
            store,
            command_history,
            started_unix,
            mouse_enabled: runtime_ui.mouse_enabled,
            open_click: runtime_ui.open_click,
            client_focus_profiles: HashMap::new(),
            client_identities,
            active_client_id: LOCAL_CLIENT_ID,
            inactive_client_states: HashMap::new(),
            should_quit: false,
            quit_confirm_deadline: None,
            needs_render: true,
            needs_full_clear: true,
            pending_api_events: Vec::new(),
            plugins: crate::plugin::PluginHost::new(),
            agent_manifests: std::sync::Arc::new(crate::agent::parse_builtin_manifests()),
            available_update: None,
            renderer: crate::ui::render::FrameRenderer::new(),
        };

        app.capture_active_client_focus_profile();

        app.persist_active_session_info();
        app.write_log("session started");

        Ok(app)
    }

    /// Load config and derive everything `App` construction needs that does
    /// not depend on where the sessions come from (fresh spawn, disk
    /// restore, or live handoff).
    fn bootstrap_from_cli(cli: Cli) -> io::Result<AppBootstrap> {
        let app_config = config::load_from_xdg()?;

        let mut options = SessionOptions::from_cli(cli.shell, cli.cwd, cli.command);
        if options.command.is_empty()
            && let Some(command) = app_config.initial_command
        {
            options.command = vec![command];
        }
        if let Some(session_name) = app_config.session_name {
            options.session_name = session_name;
        }
        options.suppress_prompt_eol_marker = app_config.shell.suppress_prompt_eol_marker;
        options.allow_passthrough = app_config.terminal.allow_passthrough;
        options.scrollback_lines = app_config.terminal.scrollback_lines;
        options.handoff_replay_bytes = app_config.pane.handoff_replay_bytes;
        options.undo_close_timeout = Duration::from_secs(app_config.pane.undo_close_seconds);
        options.new_cwd = app_config.shell.new_cwd.clone();

        let store = DataStore::from_xdg()?;
        let started_unix = unix_time_now();
        let keys = KeyMapper::with_config(
            app_config.prefix.as_deref(),
            app_config.prefix_sticky,
            &app_config.prefix_bindings,
            &app_config.global_bindings,
        );
        let runtime_ui = RuntimeUiConfig {
            keys,
            mouse_enabled: app_config.mouse.enabled,
            open_click: app_config.mouse.open_click,
            status_format: app_config
                .status
                .format
                .clone()
                .unwrap_or_else(|| DEFAULT_STATUS_FORMAT.to_string()),
            status_style: status_style_from_config(&app_config.status),
            hooks: app_config.hooks.clone(),
            editor_command: normalize_editor_command(app_config.editor.clone()),
            agent_notify: app_config.agent.notify,
            command_finish: app_config.command_finish,
            sidebar_default_open: app_config.sidebar.default_open,
            sidebar_formats: SidebarFormats::from_config(&app_config.sidebar),
            ime: app_config.ime.clone(),
        };

        Ok(AppBootstrap {
            options,
            store,
            runtime_ui,
            started_unix,
        })
    }

    fn restore_from_runtime_state(
        store: &DataStore,
        started_unix: u64,
        session_template: SessionOptions,
        runtime_ui: RuntimeUiConfig,
        cols: u16,
        rows: u16,
    ) -> io::Result<Option<Self>> {
        let state = match store.read_runtime_state::<AppRuntimeState>() {
            Ok(Some(state)) => state,
            Ok(None) | Err(_) => return Ok(None),
        };
        if state.version != RUNTIME_STATE_VERSION {
            return Ok(None);
        }

        // Any restore failure falls back to a fresh session, preserving the
        // pre-refactor behavior of this path.
        match Self::build_from_runtime_state(
            store,
            started_unix,
            session_template,
            runtime_ui,
            cols,
            rows,
            state,
            std::sync::Arc::new(crate::session::pty_backend::PtyPaneFactory),
        ) {
            Ok(app) => Ok(Some(app)),
            Err(_) => Ok(None),
        }
    }

    /// Reconstruct an `App` from a runtime-state snapshot, spawning each
    /// pane through `pane_factory` (real PTYs on disk restore, adopted fds
    /// on live handoff).
    #[allow(clippy::too_many_arguments)]
    fn build_from_runtime_state(
        store: &DataStore,
        started_unix: u64,
        session_template: SessionOptions,
        runtime_ui: RuntimeUiConfig,
        cols: u16,
        rows: u16,
        state: AppRuntimeState,
        pane_factory: std::sync::Arc<dyn crate::session::pty_backend::PaneFactory>,
    ) -> io::Result<Self> {
        let mut sessions = Vec::new();
        for session_state in state.sessions {
            let mut options = session_template.clone();
            options.session_name = session_state.session.session_name.clone();
            options.session_id = session_state.session_id.clone();
            let session = SessionManager::with_factory_from_runtime_snapshot(
                options,
                std::sync::Arc::clone(&pane_factory),
                session_state.session,
                cols,
                rows,
            )?;
            sessions.push(ManagedSession {
                ordinal: session_state.ordinal,
                session_id: session_state.session_id,
                session,
                window_names: session_state.window_names,
                pane_names: session_state.pane_names,
                window_auto_names: HashMap::new(),
                pane_auto_names: HashMap::new(),
                terminal_titles: HashMap::new(),
                cwd_fallbacks: HashMap::new(),
                agents: AgentTracking::default(),
            });
        }
        if sessions.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime state has no sessions",
            ));
        }

        let active_session = state.active_session.min(sessions.len().saturating_sub(1));
        let max_ordinal = sessions
            .iter()
            .map(|managed| managed.ordinal)
            .max()
            .unwrap_or(0);
        let next_session_ordinal = state.next_session_ordinal.max(max_ordinal + 1);
        let command_history = CommandHistory::new_with_data_dir(store.base_dir().to_path_buf());
        let pane_histories_by_session = default_pane_histories_for_managed_sessions(&sessions);
        let client_identities =
            HashMap::from([(LOCAL_CLIENT_ID, LOCAL_CLIENT_FOCUS_IDENTITY.to_string())]);

        let mut app = Self {
            sessions,
            view: ClientViewState {
                keys: runtime_ui.keys.clone(),
                input_mode: InputMode::Normal,
                status_message: None,
                locked_input: false,
                mouse_drag: None,
                text_selection: None,
                selection_autoscroll: None,
                click_chain: None,
                pending_clipboard_ansi: Vec::new(),
                pending_passthrough_ansi: Vec::new(),
                pending_image_paste_request: false,
                cols,
                rows,
                active_session,
                pane_histories_by_session,
                side_window_tree_open: runtime_ui.sidebar_default_open,
                search_history: Vec::new(),
            },
            next_session_ordinal,
            session_template,
            key_template: runtime_ui.keys,
            status_format: runtime_ui.status_format,
            status_style: runtime_ui.status_style,
            sidebar_formats: runtime_ui.sidebar_formats,
            hooks: runtime_ui.hooks,
            editor_command: runtime_ui.editor_command,
            agent_notify: runtime_ui.agent_notify,
            command_finish: runtime_ui.command_finish,
            ime: runtime_ui.ime,
            prefix_input_source_switched: false,
            editor_pane_close_targets: Vec::new(),
            store: store.clone(),
            command_history,
            started_unix,
            mouse_enabled: runtime_ui.mouse_enabled,
            open_click: runtime_ui.open_click,
            client_focus_profiles: state.client_focus_profiles,
            client_identities,
            active_client_id: LOCAL_CLIENT_ID,
            inactive_client_states: HashMap::new(),
            should_quit: false,
            quit_confirm_deadline: None,
            needs_render: true,
            needs_full_clear: true,
            pending_api_events: Vec::new(),
            plugins: crate::plugin::PluginHost::new(),
            agent_manifests: std::sync::Arc::new(crate::agent::parse_builtin_manifests()),
            available_update: None,
            renderer: crate::ui::render::FrameRenderer::new(),
        };

        app.restore_active_client_focus_profile(LOCAL_CLIENT_FOCUS_IDENTITY);
        app.capture_active_client_focus_profile();

        Ok(app)
    }

    pub fn run(&mut self, stdout: &mut std::io::Stdout) -> io::Result<()> {
        // Host mouse capture starts disabled (terminal setup no longer
        // enables it) and follows the per-frame snapshot state.
        let mut host_mouse_capture = false;
        while !self.should_quit {
            if let Some(event) = poll_event_for(FRAME_DURATION_60_FPS)? {
                match event {
                    Event::Key(key) => {
                        if matches!(key.kind, KeyEventKind::Release) {
                            continue;
                        }
                        if self.handle_key(key)? == AppSignal::DetachClient {
                            self.should_quit = true;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        self.handle_resize(cols, rows)?;
                    }
                    Event::Paste(text) => {
                        let _ = self.handle_paste(text)?;
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse_event(mouse)?;
                    }
                    _ => {}
                }
            }

            self.tick();

            for ansi in self.take_pending_passthrough_ansi_for_client(LOCAL_CLIENT_ID) {
                stdout.write_all(ansi.as_bytes())?;
                stdout.flush()?;
            }

            if let Some(snapshot) = self.take_render_snapshot() {
                if snapshot.wants_mouse_capture != host_mouse_capture {
                    let sequence =
                        crate::io::terminal::mouse_capture_sequence(snapshot.wants_mouse_capture);
                    stdout.write_all(sequence.as_bytes())?;
                    host_mouse_capture = snapshot.wants_mouse_capture;
                }
                if let Some(window_title) = snapshot.window_title.as_deref() {
                    let sequence = crate::io::terminal::osc2_title_sequence(window_title);
                    stdout.write_all(sequence.as_bytes())?;
                }
                self.renderer.render_to_writer_with_status_style(
                    stdout,
                    &snapshot.frame,
                    &snapshot.status_line,
                    snapshot.status_style,
                    snapshot.cols,
                    snapshot.rows,
                    snapshot.full_clear,
                    snapshot.overlay.as_ref(),
                    snapshot.side_window_tree.as_ref(),
                )?;
            }
        }

        Ok(())
    }

    pub fn tick(&mut self) {
        let mut output_changed = false;
        let mut title_changed = false;
        let mut passthrough_by_session = Vec::new();
        let mut terminal_events_by_session = Vec::new();
        let mut agent_dirty_by_session = Vec::new();
        for (session_index, managed) in self.sessions.iter_mut().enumerate() {
            let changed_panes = managed.session.poll_output_changed_panes();
            if !changed_panes.is_empty() {
                output_changed = true;
                agent_dirty_by_session.push((session_index, changed_panes));
            }
            let passthrough = managed.session.take_passthrough_output();
            if !passthrough.is_empty() {
                passthrough_by_session.push((session_index, passthrough));
            }
            let terminal_events = managed.session.take_terminal_events();
            if !terminal_events.is_empty() {
                terminal_events_by_session.push((session_index, terminal_events));
            }
        }
        for (session_index, passthrough) in passthrough_by_session {
            self.queue_passthrough_for_session(session_index, passthrough);
        }
        for (session_index, terminal_events) in terminal_events_by_session {
            title_changed |= self.apply_terminal_events_for_session(session_index, terminal_events);
        }
        let agent_changed = self.run_agent_detection(agent_dirty_by_session, Instant::now());
        if output_changed || title_changed || agent_changed {
            self.needs_render = true;
        }

        self.close_exited_editor_panes();

        if self.current_session_mut().focused_pane_closed() {
            self.close_focused_or_quit("pane process exited");
            self.needs_render = true;
        }

        let mut expired = self.clear_expired_message();
        let now = Instant::now();
        for state in self.inactive_client_states.values_mut() {
            let stale = state
                .status_message
                .as_ref()
                .is_some_and(|message| now >= message.expires_at);
            if stale {
                state.status_message = None;
                expired = true;
            }
        }

        if expired {
            self.needs_render = true;
        }

        self.tick_selection_autoscroll(now);
        // A drag in a remote client lives in that client's stashed view
        // state (event handling swaps it in per event and back out again),
        // so the active view above never sees it; step those in their own
        // client context.
        let autoscroll_clients: Vec<ClientId> = self
            .inactive_client_states
            .iter()
            .filter(|(_, state)| state.selection_autoscroll.is_some())
            .map(|(client_id, _)| *client_id)
            .collect();
        for client_id in autoscroll_clients {
            self.with_client_context(client_id, |app| app.tick_selection_autoscroll(now));
        }
    }

    /// Earliest future instant at which [`Self::tick`] has time-based work
    /// to do: a synchronized-output hold expiring (releases a deferred
    /// render), throttled agent detection becoming due for a pending pane,
    /// or a status message expiring. `None` when no timed work is pending.
    /// The server loop bounds its poll timeout by this so it sleeps until
    /// readiness, a wake notification, or the next deadline — whichever
    /// comes first.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut deadline: Option<Instant> = None;
        let mut consider = |candidate: Instant| {
            deadline = Some(deadline.map_or(candidate, |current| current.min(candidate)));
        };

        if self.needs_render
            && let Some(hold_expiry) = self.current_session().active_window_sync_output_deadline()
        {
            consider(hold_expiry);
        }

        for managed in &self.sessions {
            for pane_id in &managed.agents.pending {
                // Mirrors the due-filter in `run_agent_detection`: throttled
                // by the per-pane interval, and additionally suppressed while
                // an external `agent.report` is fresh.
                let mut due = managed
                    .agents
                    .last_run
                    .get(pane_id)
                    .map_or(now, |last| *last + AGENT_DETECT_INTERVAL);
                if let Some(reported_at) = managed.agents.reported.get(pane_id) {
                    let report_expiry = *reported_at + REPORTED_AGENT_TTL;
                    if report_expiry > now {
                        due = due.max(report_expiry);
                    }
                }
                consider(due);
            }
        }

        if let Some(message) = &self.view.status_message {
            consider(message.expires_at);
        }
        if let Some(autoscroll) = &self.view.selection_autoscroll {
            consider(autoscroll.next_at);
        }
        for state in self.inactive_client_states.values() {
            if let Some(message) = &state.status_message {
                consider(message.expires_at);
            }
            if let Some(autoscroll) = &state.selection_autoscroll {
                consider(autoscroll.next_at);
            }
        }

        deadline
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Apply a fresh cached update-check result, if one exists. Returns
    /// true when the cache was fresh, i.e. no background check is needed.
    pub fn load_cached_update_check(&mut self) -> bool {
        let now_unix = unix_time_now() as i64;
        let Some(cache) = crate::upgrade::read_fresh_update_cache(self.store.base_dir(), now_unix)
        else {
            return false;
        };
        self.set_available_update(cache.newer_version());
        true
    }

    /// Store the outcome of a background update check: successful checks
    /// are cached and an available update is exposed to the status line;
    /// errors are logged without caching so the next startup retries.
    pub fn apply_update_check_result(&mut self, result: Result<Option<String>, String>) {
        match result {
            Ok(newer_version) => {
                let cache = crate::upgrade::UpdateCheckCache::from_check_result(
                    unix_time_now() as i64,
                    newer_version.as_deref(),
                );
                if let Err(err) = crate::upgrade::write_update_cache(self.store.base_dir(), &cache)
                {
                    self.write_log(&format!("update check cache write failed: {err}"));
                }
                self.set_available_update(newer_version);
            }
            Err(err) => self.write_log(&format!("update check failed: {err}")),
        }
    }

    fn set_available_update(&mut self, version: Option<String>) {
        if version.is_some() {
            self.request_render(false);
        }
        self.available_update = version;
    }

    /// `{update}` status token: `update available: vX.Y.Z` when a newer
    /// release is known, empty otherwise.
    fn update_token(&self) -> String {
        self.available_update
            .as_ref()
            .map(|version| format!("update available: v{version}"))
            .unwrap_or_default()
    }

    pub fn apply_attach_target(&mut self, target: &AttachTarget) -> Result<(), String> {
        let session_index = self.resolve_session_index_for_attach(&target.session_token)?;
        let window_entries = self.sessions[session_index].session.window_entries();
        self.ensure_target_window_exists(target, &window_entries)?;
        let pane_id = self.resolve_target_pane_id(target, &window_entries)?;

        self.view.active_session = session_index;
        self.restore_focus_for_active_session_from_history();
        if let Some(window_number) = target.window {
            self.current_session_mut()
                .focus_window_number(window_number)
                .map_err(|err| format!("focus window w{window_number} failed: {err}"))?;
            self.record_focus_for_active_session();
        }
        if let Some(pane_id) = pane_id {
            self.current_session_mut()
                .focus_pane_id(pane_id)
                .map_err(|err| format!("focus pane p{pane_id} failed: {err}"))?;
            self.record_focus_for_active_session();
        } else {
            self.sync_focus_history_for_active_session();
        }

        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        Ok(())
    }

    fn ensure_target_window_exists(
        &self,
        target: &AttachTarget,
        window_entries: &[crate::session::manager::WindowEntry],
    ) -> Result<(), String> {
        if let Some(window_number) = target.window
            && !window_entries
                .iter()
                .any(|entry| entry.index == window_number)
        {
            return Err(format!(
                "window w{window_number} not found in session `{}`",
                target.session_token
            ));
        }
        Ok(())
    }

    fn resolve_target_pane_id(
        &self,
        target: &AttachTarget,
        window_entries: &[crate::session::manager::WindowEntry],
    ) -> Result<Option<usize>, String> {
        let Some(pane_selector) = target.pane else {
            return Ok(None);
        };

        if target.pane_is_index {
            let Some(window_number) = target.window else {
                return Err("pane index requires a window segment".to_string());
            };
            let Some(window_entry) = window_entries
                .iter()
                .find(|entry| entry.index == window_number)
            else {
                return Err(format!(
                    "window w{window_number} not found in session `{}`",
                    target.session_token
                ));
            };
            let Some(pane_offset) = pane_selector.checked_sub(1) else {
                return Err("pane index must be >= 1".to_string());
            };
            let Some(pane_id) = window_entry.pane_ids.get(pane_offset).copied() else {
                return Err(format!(
                    "pane index i{pane_selector} not found in window w{window_number}"
                ));
            };
            return Ok(Some(pane_id));
        }

        let pane_id = pane_selector;
        let Some(window_for_pane) = window_entries
            .iter()
            .find(|entry| entry.pane_ids.contains(&pane_id))
            .map(|entry| entry.index)
        else {
            return Err(format!(
                "pane p{pane_id} not found in session `{}`",
                target.session_token
            ));
        };

        if let Some(window_number) = target.window
            && window_for_pane != window_number
        {
            return Err(format!("pane p{pane_id} is not in window w{window_number}"));
        }

        Ok(Some(pane_id))
    }

    fn current_session(&self) -> &SessionManager {
        &self.sessions[self.view.active_session].session
    }

    fn current_session_mut(&mut self) -> &mut SessionManager {
        &mut self.sessions[self.view.active_session].session
    }

    fn current_session_id(&self) -> &str {
        &self.sessions[self.view.active_session].session_id
    }

    fn effective_window_name(&self, session_index: usize, window_id: WindowId) -> Option<&str> {
        let managed = self.sessions.get(session_index)?;
        managed
            .window_names
            .get(&window_id)
            .or_else(|| managed.window_auto_names.get(&window_id))
            .map(String::as_str)
    }

    fn effective_pane_name(&self, session_index: usize, pane_id: usize) -> Option<&str> {
        let managed = self.sessions.get(session_index)?;
        managed
            .pane_names
            .get(&pane_id)
            .or_else(|| managed.pane_auto_names.get(&pane_id))
            .map(String::as_str)
    }

    fn set_name<K: std::cmp::Eq + std::hash::Hash + Copy>(
        names: &mut HashMap<K, String>,
        key: K,
        next_name: Option<String>,
    ) -> bool {
        let next_name = next_name.filter(|value| !value.is_empty());
        match next_name {
            Some(next_name) => {
                if names.get(&key).is_some_and(|current| current == &next_name) {
                    false
                } else {
                    names.insert(key, next_name);
                    true
                }
            }
            None => names.remove(&key).is_some(),
        }
    }

    fn resolve_auto_pane_name(managed: &ManagedSession, pane_id: usize) -> Option<String> {
        managed
            .terminal_titles
            .get(&pane_id)
            .cloned()
            .or_else(|| managed.cwd_fallbacks.get(&pane_id).cloned())
    }

    fn focused_window_title_from_terminal_events(&self) -> Option<String> {
        let managed = self.sessions.get(self.view.active_session)?;
        let pane_id = managed.session.focused_pane_id()?;
        Self::resolve_auto_pane_name(managed, pane_id)
    }

    fn apply_terminal_events_for_session(
        &mut self,
        session_index: usize,
        events: Vec<PaneTerminalEvent>,
    ) -> bool {
        let Some(managed) = self.sessions.get_mut(session_index) else {
            return false;
        };

        let mut changed = false;
        let mut clipboard_texts = Vec::new();
        let mut notifications = Vec::new();
        let mut progress_updates = Vec::new();
        let mut finished_commands = Vec::new();
        for pane_event in events {
            let pane_id = pane_event.pane_id;
            match pane_event.event {
                TerminalEvent::TitleChanged { title } => {
                    changed |= Self::set_name(&mut managed.terminal_titles, pane_id, title);
                }
                TerminalEvent::CwdChanged { cwd } => {
                    changed |= Self::set_name(&mut managed.cwd_fallbacks, pane_id, Some(cwd));
                }
                TerminalEvent::ClipboardSet { text } => {
                    clipboard_texts.push(text);
                    continue;
                }
                TerminalEvent::Notification { message } => {
                    notifications.push(message);
                    continue;
                }
                TerminalEvent::ProgressChanged { progress } => {
                    progress_updates.push(progress);
                    continue;
                }
                TerminalEvent::CommandStarted => continue,
                TerminalEvent::CommandFinished { duration, .. } => {
                    finished_commands.push((pane_id, duration));
                    continue;
                }
            }

            let auto_name = Self::resolve_auto_pane_name(managed, pane_id);
            changed |= Self::set_name(&mut managed.pane_auto_names, pane_id, auto_name.clone());
            if let Some(window_id) = managed.session.window_id_for_pane(pane_id) {
                changed |= Self::set_name(&mut managed.window_auto_names, window_id, auto_name);
            }
        }

        for text in clipboard_texts {
            self.broadcast_clipboard_to_clients(&text);
        }
        for message in notifications {
            self.broadcast_notification_to_clients(&message);
        }
        for progress in progress_updates {
            self.broadcast_progress_to_clients(progress);
        }
        for (pane_id, duration) in finished_commands {
            self.ring_bell_on_command_finished(session_index, pane_id, duration);
        }

        changed
    }

    /// Ghostty-style notify-on-command-finish: when a guest command marked
    /// by OSC 133;C→D ran at least `[command_finish] min_duration_ms`, ring
    /// the host terminal bell of attached clients. Only a BEL is sent — the
    /// host terminal's own bell features (sound, urgency hint, title badge)
    /// take it from there. `duration` is `None` for a 133;D without a
    /// matching 133;C (e.g. integrations that emit D on empty prompts);
    /// those never ring. In `unfocused` mode a client viewing the pane is
    /// skipped: the active client's viewed pane is the session's focused
    /// pane, an inactive client's is the head of its per-session focus
    /// history (captured when it was last active — focus can't move while
    /// a client is inactive).
    fn ring_bell_on_command_finished(
        &mut self,
        session_index: usize,
        pane_id: usize,
        duration: Option<Duration>,
    ) {
        use config::CommandFinishNotifyMode as Mode;

        if self.command_finish.notify == Mode::Off {
            return;
        }
        let Some(duration) = duration else {
            return;
        };
        if duration < Duration::from_millis(self.command_finish.min_duration_ms) {
            return;
        }
        let Some(managed) = self.sessions.get(session_index) else {
            return;
        };
        let session_id = managed.session_id.clone();
        let focused_pane = managed.session.focused_pane_id();
        let ring_all = self.command_finish.notify == Mode::Always;

        let active_viewing =
            self.view.active_session == session_index && focused_pane == Some(pane_id);
        if ring_all || !active_viewing {
            self.view.pending_passthrough_ansi.push(BELL.to_string());
        }
        for state in self.inactive_client_states.values_mut() {
            let viewing = state.active_session == session_index
                && state
                    .pane_histories_by_session
                    .get(&session_id)
                    .and_then(|history| history.current_pane())
                    == Some(pane_id);
            if ring_all || !viewing {
                state.pending_passthrough_ansi.push(BELL.to_string());
            }
        }
    }

    /// Queue an OSC 52 clipboard frame for every attached client so a
    /// guest-initiated clipboard write (OSC 52) reaches each client's host
    /// terminal, mirroring how tmux forwards set-clipboard.
    fn broadcast_clipboard_to_clients(&mut self, text: &str) {
        let sequence = crate::clipboard::osc52_sequence(text);
        self.view.pending_clipboard_ansi.push(sequence.clone());
        for state in self.inactive_client_states.values_mut() {
            state.pending_clipboard_ansi.push(sequence.clone());
        }
    }

    /// Queue an OSC 9 desktop-notification frame for every attached
    /// client's host terminal, mirroring the OSC 52 clipboard broadcast.
    /// Delivered over the passthrough channel, which the server loop
    /// drains on every pass.
    fn broadcast_notification_to_clients(&mut self, message: &str) {
        let sequence = crate::io::terminal::osc9_notification_sequence(message);
        self.view.pending_passthrough_ansi.push(sequence.clone());
        for state in self.inactive_client_states.values_mut() {
            state.pending_passthrough_ansi.push(sequence.clone());
        }
    }

    /// Queue a ConEmu OSC 9;4 progress frame for every attached client's
    /// host terminal. Reports from any pane are forwarded (most recent
    /// wins) so a finishing command's remove always reaches the host;
    /// concurrent progress in multiple panes may interleave.
    fn broadcast_progress_to_clients(
        &mut self,
        progress: Option<crate::session::terminal_state::ProgressReport>,
    ) {
        let sequence = crate::io::terminal::osc94_progress_sequence(progress);
        self.view.pending_passthrough_ansi.push(sequence.clone());
        for state in self.inactive_client_states.values_mut() {
            state.pending_passthrough_ansi.push(sequence.clone());
        }
    }

    fn session_index_for_id(&self, session_id: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|managed| managed.session_id == session_id)
    }

    fn set_message(&mut self, text: &str, ttl: Duration) {
        self.view.status_message = Some(TimedMessage {
            text: text.to_string(),
            expires_at: Instant::now() + ttl,
        });
    }

    fn clear_expired_message(&mut self) -> bool {
        let expired = self
            .view
            .status_message
            .as_ref()
            .is_some_and(|message| Instant::now() >= message.expires_at);
        if expired {
            self.view.status_message = None;
        }
        expired
    }

    fn resize_sessions_to_max_client_viewport(&mut self) -> io::Result<()> {
        let mut max_cols =
            Self::effective_pane_cols_for_view(self.view.cols, self.view.side_window_tree_open);
        let mut max_rows = self.view.rows;
        for state in self.inactive_client_states.values() {
            max_cols = max_cols.max(Self::effective_pane_cols_for_view(
                state.cols,
                state.side_window_tree_open,
            ));
            max_rows = max_rows.max(state.rows);
        }
        for managed in &mut self.sessions {
            managed.session.resize(max_cols, max_rows)?;
        }
        Ok(())
    }

    fn handle_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.view.cols = cols;
        self.view.rows = rows;
        self.resize_sessions_to_max_client_viewport()?;
        self.needs_render = true;
        Ok(())
    }

    fn queue_passthrough_for_session(&mut self, session_index: usize, chunks: Vec<Vec<u8>>) {
        if chunks.is_empty() {
            return;
        }
        let sequences = chunks
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<Vec<_>>();
        if sequences.is_empty() {
            return;
        }
        if self.view.active_session == session_index {
            self.view
                .pending_passthrough_ansi
                .extend(sequences.iter().cloned());
        }
        for state in self.inactive_client_states.values_mut() {
            if state.active_session == session_index {
                state
                    .pending_passthrough_ansi
                    .extend(sequences.iter().cloned());
            }
        }
    }
}

fn default_pane_histories_for_managed_sessions(
    sessions: &[ManagedSession],
) -> HashMap<String, PaneFocusHistory> {
    let mut histories = HashMap::new();
    for managed in sessions {
        let Some(focused) = managed.session.focused_pane_id() else {
            continue;
        };
        let mut history = PaneFocusHistory::default();
        history.record_focus(focused);
        histories.insert(managed.session_id.clone(), history);
    }
    histories
}

fn prune_pane_histories_for_managed_sessions(
    histories: &mut HashMap<String, PaneFocusHistory>,
    sessions: &[ManagedSession],
) {
    let valid_panes_by_session = sessions
        .iter()
        .map(|managed| {
            (
                managed.session_id.clone(),
                managed
                    .session
                    .all_pane_ids()
                    .into_iter()
                    .collect::<HashSet<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();

    histories.retain(|session_id, history| {
        let Some(valid_panes) = valid_panes_by_session.get(session_id) else {
            return false;
        };
        history.prune_invalid(valid_panes);
        !history.is_empty()
    });
}

fn persisted_client_focus_state_from_state(
    active_session: usize,
    pane_histories_by_session: &HashMap<String, PaneFocusHistory>,
    sessions: &[ManagedSession],
) -> PersistedClientFocusState {
    let mut pane_histories_by_session = pane_histories_by_session.clone();
    prune_pane_histories_for_managed_sessions(&mut pane_histories_by_session, sessions);
    PersistedClientFocusState {
        active_session_id: sessions
            .get(active_session)
            .map(|managed| managed.session_id.clone()),
        pane_histories_by_session: pane_histories_by_session
            .into_iter()
            .map(|(session_id, history)| (session_id, history.snapshot()))
            .collect(),
    }
}

fn normalize_client_identity(identity: Option<String>) -> Option<String> {
    let value = identity?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(super) fn session_id_for(session_name: &str, ordinal: usize) -> String {
    format!(
        "{}-{ordinal}",
        DataStore::normalize_session_id(session_name)
    )
}

fn parse_session_alias(token: &str) -> Option<usize> {
    let number = token.strip_prefix('s')?;
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let parsed = number.parse::<usize>().ok()?;
    (parsed >= 1).then_some(parsed)
}

fn is_closed_pane_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::NotConnected
    ) || err.raw_os_error() == Some(5)
}
