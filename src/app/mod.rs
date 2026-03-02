mod command_dispatch;
mod command_palette;
mod copy_mode;
mod hooks;
mod persistence;
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
use crate::input::{CommandAction, InputAction, KeyMapper, encode_key_to_bytes};
use crate::session::manager::SessionOptions;
use crate::session::manager::{PaneTerminalEvent, SessionManager};
use crate::session::terminal_state::{CellStyle, TerminalEvent};
use crate::storage::{DataStore, unix_time_now};
use crate::ui::window_manager::{Direction, WindowId};
use crate::core_lib::runtime::event_loop::{FRAME_DURATION_60_FPS, poll_event_for};
use types::*;

pub type ClientId = u64;
pub const LOCAL_CLIENT_ID: ClientId = 0;

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
}

pub struct App {
    sessions: Vec<ManagedSession>,
    view: ClientViewState,
    next_session_ordinal: usize,
    session_template: SessionOptions,
    key_template: KeyMapper,
    status_format: String,
    status_style: CellStyle,
    hooks: config::HooksConfig,
    editor_command: Option<String>,
    editor_pane_close_targets: Vec<EditorPaneCloseTarget>,
    store: DataStore,
    command_history: CommandHistory,
    started_unix: u64,
    mouse_enabled: bool,
    client_focus_profiles: HashMap<String, PersistedClientFocusState>,
    client_identities: HashMap<ClientId, String>,
    active_client_id: ClientId,
    inactive_client_states: HashMap<ClientId, ClientViewState>,
    should_quit: bool,
    needs_render: bool,
    needs_full_clear: bool,
    renderer: crate::ui::render::FrameRenderer,
}

impl App {
    pub fn new(cli: Cli) -> io::Result<Self> {
        let (cols, rows) = crossterm::terminal::size()?;
        Self::new_with_size(cli, cols, rows)
    }

    pub fn new_with_size(cli: Cli, cols: u16, rows: u16) -> io::Result<Self> {
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

        let store = DataStore::from_xdg()?;
        let command_history = CommandHistory::new_with_data_dir(store.base_dir().to_path_buf());
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
            status_format: app_config
                .status
                .format
                .clone()
                .unwrap_or_else(|| DEFAULT_STATUS_FORMAT.to_string()),
            status_style: status_style_from_config(&app_config.status),
            hooks: app_config.hooks.clone(),
            editor_command: normalize_editor_command(app_config.editor.clone()),
        };

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

        let mut session = SessionManager::new(options.clone(), cols, rows)?;
        session.resize(cols, rows)?;

        let first_ordinal = 1;
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
                pending_clipboard_ansi: Vec::new(),
                pending_passthrough_ansi: Vec::new(),
                cols,
                rows,
                active_session: 0,
                pane_histories_by_session,
                side_window_tree_open: false,
            },
            next_session_ordinal: 2,
            session_template: options,
            key_template: runtime_ui.keys,
            status_format: runtime_ui.status_format,
            status_style: runtime_ui.status_style,
            hooks: runtime_ui.hooks,
            editor_command: runtime_ui.editor_command,
            editor_pane_close_targets: Vec::new(),
            store,
            command_history,
            started_unix,
            mouse_enabled: runtime_ui.mouse_enabled,
            client_focus_profiles: HashMap::new(),
            client_identities,
            active_client_id: LOCAL_CLIENT_ID,
            inactive_client_states: HashMap::new(),
            should_quit: false,
            needs_render: true,
            needs_full_clear: true,
            renderer: crate::ui::render::FrameRenderer::new(),
        };

        app.capture_active_client_focus_profile();

        app.persist_active_session_info();
        app.write_log("session started");

        Ok(app)
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

        let mut sessions = Vec::new();
        for session_state in state.sessions {
            let mut options = session_template.clone();
            options.session_name = session_state.session.session_name.clone();
            let session = match SessionManager::from_runtime_snapshot(
                options,
                session_state.session,
                cols,
                rows,
            ) {
                Ok(session) => session,
                Err(_) => return Ok(None),
            };
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
            });
        }
        if sessions.is_empty() {
            return Ok(None);
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
                pending_clipboard_ansi: Vec::new(),
                pending_passthrough_ansi: Vec::new(),
                cols,
                rows,
                active_session,
                pane_histories_by_session,
                side_window_tree_open: false,
            },
            next_session_ordinal,
            session_template,
            key_template: runtime_ui.keys,
            status_format: runtime_ui.status_format,
            status_style: runtime_ui.status_style,
            hooks: runtime_ui.hooks,
            editor_command: runtime_ui.editor_command,
            editor_pane_close_targets: Vec::new(),
            store: store.clone(),
            command_history,
            started_unix,
            mouse_enabled: runtime_ui.mouse_enabled,
            client_focus_profiles: state.client_focus_profiles,
            client_identities,
            active_client_id: LOCAL_CLIENT_ID,
            inactive_client_states: HashMap::new(),
            should_quit: false,
            needs_render: true,
            needs_full_clear: true,
            renderer: crate::ui::render::FrameRenderer::new(),
        };

        app.restore_active_client_focus_profile(LOCAL_CLIENT_FOCUS_IDENTITY);
        app.capture_active_client_focus_profile();

        Ok(Some(app))
    }

    pub fn run(&mut self, stdout: &mut std::io::Stdout) -> io::Result<()> {
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
        for (session_index, managed) in self.sessions.iter_mut().enumerate() {
            output_changed |= managed.session.poll_output();
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
        if output_changed || title_changed {
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
    }

    pub fn take_render_snapshot(&mut self) -> Option<RenderSnapshot> {
        let snapshot = self.render_snapshot_for_client(LOCAL_CLIENT_ID)?;
        self.finish_render_cycle();
        Some(snapshot)
    }

    pub fn has_pending_render(&self) -> bool {
        self.needs_render
    }

    pub fn render_snapshot_for_client(&mut self, client_id: ClientId) -> Option<RenderSnapshot> {
        if !self.needs_render {
            return None;
        }

        let snapshot = self.with_client_context(client_id, |app| {
            let side_window_tree = app.side_window_tree_overlay();
            let mut frame = app.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
            // Apply text selection highlighting to pane cells
            if let Some(sel) = &app.view.text_selection {
                Self::apply_selection_highlight(&mut frame, sel);
            }
            if let InputMode::CursorMode { state } = &app.view.input_mode {
                Self::apply_cursor_mode_frame(&mut frame, state);
            }
            RenderSnapshot {
                frame,
                status_line: app.status_line(),
                status_style: app.status_style,
                window_title: app.focused_window_title_from_terminal_events(),
                overlay: app.system_overlay(),
                side_window_tree,
                cols: app.view.cols,
                rows: app.view.rows,
                full_clear: app.needs_full_clear,
            }
        });
        Some(snapshot)
    }

    pub fn finish_render_cycle(&mut self) {
        self.needs_render = false;
        self.needs_full_clear = false;
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn request_render(&mut self, full_clear: bool) {
        self.needs_render = true;
        if full_clear {
            self.needs_full_clear = true;
        }
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
        for pane_event in events {
            let pane_id = pane_event.pane_id;
            match pane_event.event {
                TerminalEvent::TitleChanged { title } => {
                    changed |= Self::set_name(&mut managed.terminal_titles, pane_id, title);
                }
                TerminalEvent::CwdChanged { cwd } => {
                    changed |= Self::set_name(&mut managed.cwd_fallbacks, pane_id, Some(cwd));
                }
            }

            let auto_name = Self::resolve_auto_pane_name(managed, pane_id);
            changed |= Self::set_name(&mut managed.pane_auto_names, pane_id, auto_name.clone());
            if let Some(window_id) = managed.session.window_id_for_pane(pane_id) {
                changed |= Self::set_name(&mut managed.window_auto_names, window_id, auto_name);
            }
        }

        changed
    }

    fn prune_side_window_tree_state(&mut self) {
        if self.sessions.is_empty() {
            self.view.side_window_tree_open = false;
        }
    }

    fn side_window_tree_is_open(&self) -> bool {
        self.view.side_window_tree_open
    }

    fn side_window_tree_selected_index(&self) -> Option<usize> {
        let windows = self.current_session().window_entries();
        if windows.is_empty() {
            return None;
        }
        Some(windows.iter().position(|entry| entry.focused).unwrap_or(0))
    }

    fn side_window_tree_scroll_start(selected: usize, total: usize, visible: usize) -> usize {
        if total == 0 || visible == 0 || total <= visible {
            return 0;
        }
        let max_start = total - visible;
        selected
            .saturating_add(1)
            .saturating_sub(visible)
            .min(max_start)
    }

    fn side_window_tree_width_for_cols(cols: u16) -> Option<u16> {
        let cols = usize::from(cols);
        if cols < 30 {
            return None;
        }
        let preferred = ((cols * 28) / 100).clamp(18, 28);
        let max_width = cols.saturating_sub(12);
        if max_width < 12 {
            return None;
        }
        Some(preferred.min(max_width) as u16)
    }

    fn effective_pane_cols_for_view(cols: u16, side_window_tree_open: bool) -> u16 {
        let sidebar_width = if side_window_tree_open {
            Self::side_window_tree_width_for_cols(cols).unwrap_or(0)
        } else {
            0
        };
        cols.saturating_sub(sidebar_width)
    }

    fn current_effective_pane_dims(&self) -> (u16, u16) {
        (
            Self::effective_pane_cols_for_view(self.view.cols, self.view.side_window_tree_open),
            self.view.rows,
        )
    }

    fn side_window_tree_width(&self) -> Option<usize> {
        Self::side_window_tree_width_for_cols(self.view.cols).map(usize::from)
    }

    fn side_window_tree_overlay(&self) -> Option<crate::ui::render::SideWindowTree> {
        if !self.side_window_tree_is_open() {
            return None;
        }
        let width = self.side_window_tree_width()?;
        let managed = self.sessions.get(self.view.active_session)?;
        let windows = managed.session.window_entries();
        if windows.is_empty() {
            return None;
        }
        let selected = self
            .side_window_tree_selected_index()
            .unwrap_or(0)
            .min(windows.len().saturating_sub(1));
        let entries = windows
            .iter()
            .map(|entry| {
                let custom_name = self
                    .effective_window_name(self.view.active_session, entry.window_id)
                    .filter(|name| !name.is_empty());
                if let Some(name) = custom_name {
                    format!("w{}:{name}", entry.index)
                } else {
                    format!("w{}", entry.index)
                }
            })
            .collect::<Vec<_>>();

        Some(crate::ui::render::SideWindowTree {
            title: "windows".to_string(),
            entries,
            selected,
            width,
        })
    }

    fn side_window_tree_window_number_at(
        &self,
        side: &crate::ui::render::SideWindowTree,
        col: u16,
        row: u16,
    ) -> Option<usize> {
        let col = usize::from(col);
        let row = usize::from(row);
        let workspace_rows = usize::from(self.view.rows.saturating_sub(1));

        // Clicks must land on entry text area, not the divider/header/status rows.
        if workspace_rows <= 1
            || col >= side.width.saturating_sub(1)
            || row == 0
            || row >= workspace_rows
        {
            return None;
        }

        let windows = self.current_session().window_entries();
        if windows.is_empty() {
            return None;
        }

        let content_height = workspace_rows.saturating_sub(1);
        if content_height == 0 {
            return None;
        }

        let selected = self
            .side_window_tree_selected_index()
            .unwrap_or(0)
            .min(windows.len().saturating_sub(1));
        let start = Self::side_window_tree_scroll_start(selected, windows.len(), content_height);
        let entry_index = start + row.saturating_sub(1);

        windows.get(entry_index).map(|entry| entry.index)
    }

    fn shift_frame_for_side_window_tree(
        &self,
        frame: &mut crate::session::manager::RenderFrame,
        width: usize,
    ) {
        if width == 0 {
            return;
        }
        let offset = width as u16;
        for pane in &mut frame.panes {
            pane.rect.x = pane.rect.x.saturating_add(width);
        }
        for divider in &mut frame.dividers {
            divider.x = divider.x.saturating_add(width);
        }
        if let Some((x, y)) = frame.focused_cursor {
            frame.focused_cursor = Some((x.saturating_add(offset), y));
        }
    }

    fn pane_frame_for_current_view_with_sidebar(
        &self,
        side_window_tree: Option<&crate::ui::render::SideWindowTree>,
    ) -> crate::session::manager::RenderFrame {
        let pane_cols = self.current_effective_pane_dims().0;
        let mut frame = if matches!(&self.view.input_mode, InputMode::PeekAllWindows { .. }) {
            self.current_session()
                .peek_all_panes_frame(pane_cols, self.view.rows)
        } else {
            self.current_session().frame(pane_cols, self.view.rows)
        };
        if let Some(tree) = side_window_tree {
            self.shift_frame_for_side_window_tree(&mut frame, tree.width);
        }
        frame
    }

    fn toggle_side_window_tree(&mut self) {
        self.prune_side_window_tree_state();
        self.view.side_window_tree_open = !self.view.side_window_tree_open;
        if let Err(err) = self.resize_sessions_to_max_client_viewport() {
            self.set_message(&format!("resize failed: {err}"), Duration::from_secs(3));
            self.write_log(&format!("side window tree resize failed: {err}"));
        }
        self.needs_full_clear = true;
    }

    fn session_index_for_id(&self, session_id: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|managed| managed.session_id == session_id)
    }

    fn sync_focus_history_for_active_session(&mut self) {
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let focused = self.current_session().focused_pane_id();

        let should_remove = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id.clone())
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            if let Some(pane_id) = focused {
                history.sync_index_from_current(pane_id);
            }
            history.is_empty()
        };

        if should_remove {
            self.view.pane_histories_by_session.remove(&session_id);
        }
    }

    fn record_focus_for_active_session(&mut self) {
        let Some(focused) = self.current_session().focused_pane_id() else {
            return;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let history = self
            .view
            .pane_histories_by_session
            .entry(session_id)
            .or_default();
        history.prune_invalid(&valid_pane_ids);
        history.record_focus(focused);
    }

    fn restore_focus_for_active_session_from_history(&mut self) {
        if self.sessions.is_empty() {
            return;
        }

        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let focused = self.current_session().focused_pane_id();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            if history.is_empty() {
                if let Some(pane_id) = focused {
                    history.record_focus(pane_id);
                }
                None
            } else {
                history
                    .current_pane()
                    .or(focused)
                    .or_else(|| history.pane_ids.last().copied())
            }
        };

        if let Some(target_pane_id) = target {
            let _ = self.current_session_mut().focus_pane_id(target_pane_id);
        }
        self.sync_focus_history_for_active_session();
    }

    fn focus_prev_pane_history(&mut self) -> bool {
        let Some(current_pane_id) = self.current_session().focused_pane_id() else {
            return false;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            history.sync_index_from_current(current_pane_id);
            history.prev_from(current_pane_id)
        };
        let Some(target_pane_id) = target else {
            return false;
        };

        if self
            .current_session_mut()
            .focus_pane_id(target_pane_id)
            .is_err()
        {
            return false;
        }
        self.sync_focus_history_for_active_session();
        self.current_session().focused_pane_id() != Some(current_pane_id)
    }

    fn focus_next_pane_history(&mut self) -> bool {
        let Some(current_pane_id) = self.current_session().focused_pane_id() else {
            return false;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            history.sync_index_from_current(current_pane_id);
            history.next_from(current_pane_id)
        };
        let Some(target_pane_id) = target else {
            return false;
        };

        if self
            .current_session_mut()
            .focus_pane_id(target_pane_id)
            .is_err()
        {
            return false;
        }
        self.sync_focus_history_for_active_session();
        self.current_session().focused_pane_id() != Some(current_pane_id)
    }

    fn restore_active_client_focus_profile(&mut self, identity: &str) {
        let Some(profile) = self.client_focus_profiles.get(identity).cloned() else {
            return;
        };
        self.apply_persisted_client_focus_state(profile);
    }

    fn restore_client_focus_profile_for_client(&mut self, client_id: ClientId, identity: &str) {
        let Some(profile) = self.client_focus_profiles.get(identity).cloned() else {
            return;
        };
        self.with_client_context(client_id, move |app| {
            app.apply_persisted_client_focus_state(profile);
        });
    }

    fn apply_persisted_client_focus_state(&mut self, profile: PersistedClientFocusState) {
        let mut pane_histories_by_session =
            default_pane_histories_for_managed_sessions(&self.sessions);
        for (session_id, history) in profile.pane_histories_by_session {
            pane_histories_by_session.insert(session_id, PaneFocusHistory::from_snapshot(history));
        }
        prune_pane_histories_for_managed_sessions(&mut pane_histories_by_session, &self.sessions);
        self.view.pane_histories_by_session = pane_histories_by_session;

        if let Some(session_id) = profile.active_session_id
            && let Some(session_index) = self.session_index_for_id(&session_id)
        {
            self.view.active_session = session_index;
        }

        self.restore_focus_for_active_session_from_history();
    }

    fn capture_client_focus_profile(&mut self, client_id: ClientId) {
        if client_id == self.active_client_id {
            self.capture_active_client_focus_profile();
            return;
        }

        let Some(identity) = self.client_identities.get(&client_id).cloned() else {
            return;
        };
        let Some(state) = self.inactive_client_states.get(&client_id) else {
            return;
        };
        let profile = persisted_client_focus_state_from_state(
            state.active_session,
            &state.pane_histories_by_session,
            &self.sessions,
        );
        self.client_focus_profiles.insert(identity, profile);
    }

    fn capture_active_client_focus_profile(&mut self) {
        self.sync_focus_history_for_active_session();
        let Some(identity) = self.client_identities.get(&self.active_client_id).cloned() else {
            return;
        };
        let profile = persisted_client_focus_state_from_state(
            self.view.active_session,
            &self.view.pane_histories_by_session,
            &self.sessions,
        );
        self.client_focus_profiles.insert(identity, profile);
    }

    fn collect_client_focus_profiles(&self) -> HashMap<String, PersistedClientFocusState> {
        let mut profiles = self.client_focus_profiles.clone();

        if let Some(identity) = self.client_identities.get(&self.active_client_id) {
            let profile = persisted_client_focus_state_from_state(
                self.view.active_session,
                &self.view.pane_histories_by_session,
                &self.sessions,
            );
            profiles.insert(identity.clone(), profile);
        }

        for (client_id, state) in &self.inactive_client_states {
            let Some(identity) = self.client_identities.get(client_id) else {
                continue;
            };
            let profile = persisted_client_focus_state_from_state(
                state.active_session,
                &state.pane_histories_by_session,
                &self.sessions,
            );
            profiles.insert(identity.clone(), profile);
        }

        profiles.retain(|_, profile| {
            profile.active_session_id.is_some() || !profile.pane_histories_by_session.is_empty()
        });
        profiles
    }

    fn default_client_view_state(&self, cols: u16, rows: u16) -> ClientViewState {
        ClientViewState {
            keys: self.key_template.clone(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols,
            rows,
            active_session: self.view.active_session,
            pane_histories_by_session: self.view.pane_histories_by_session.clone(),
            side_window_tree_open: self.view.side_window_tree_open,
        }
    }

    fn take_active_client_state(&mut self) -> ClientViewState {
        self.sync_focus_history_for_active_session();
        let reset = self.default_client_view_state_reset();
        std::mem::replace(&mut self.view, reset)
    }

    fn default_client_view_state_reset(&self) -> ClientViewState {
        ClientViewState {
            keys: self.key_template.clone(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            cols: self.view.cols,
            rows: self.view.rows,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
        }
    }

    fn install_active_client_state(&mut self, mut state: ClientViewState) {
        prune_pane_histories_for_managed_sessions(
            &mut state.pane_histories_by_session,
            &self.sessions,
        );
        let max_session_index = self.sessions.len().saturating_sub(1);
        state.active_session = state.active_session.min(max_session_index);
        self.view = state;
        self.prune_side_window_tree_state();
        self.restore_focus_for_active_session_from_history();
    }

    fn switch_active_client(&mut self, client_id: ClientId) {
        if self.active_client_id == client_id {
            return;
        }

        let previous_id = self.active_client_id;
        self.capture_active_client_focus_profile();
        let previous_state = self.take_active_client_state();
        self.inactive_client_states
            .insert(previous_id, previous_state);

        let next_state = self
            .inactive_client_states
            .remove(&client_id)
            .unwrap_or_else(|| self.default_client_view_state(self.view.cols, self.view.rows));
        self.install_active_client_state(next_state);
        self.active_client_id = client_id;
        self.capture_active_client_focus_profile();
    }

    fn with_client_context<T>(
        &mut self,
        client_id: ClientId,
        action: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous_client_id = self.active_client_id;
        self.switch_active_client(client_id);
        let result = action(self);
        self.switch_active_client(previous_client_id);
        result
    }

    pub fn register_client(&mut self, client_id: ClientId, cols: u16, rows: u16) {
        if self.active_client_id == client_id {
            self.view.cols = cols;
            self.view.rows = rows;
            return;
        }

        if let Some(state) = self.inactive_client_states.get_mut(&client_id) {
            state.cols = cols;
            state.rows = rows;
            return;
        }

        self.inactive_client_states
            .insert(client_id, self.default_client_view_state(cols, rows));
    }

    pub fn register_client_identity(&mut self, client_id: ClientId, identity: Option<String>) {
        let identity =
            normalize_client_identity(identity).unwrap_or_else(|| format!("client-{client_id}"));
        self.client_identities.insert(client_id, identity.clone());
        self.restore_client_focus_profile_for_client(client_id, &identity);
        self.capture_client_focus_profile(client_id);
    }

    pub fn unregister_client(&mut self, client_id: ClientId) {
        if self.active_client_id == client_id {
            if client_id == LOCAL_CLIENT_ID {
                return;
            }

            self.capture_active_client_focus_profile();
            let _ = self.take_active_client_state();
            let fallback = self
                .inactive_client_states
                .remove(&LOCAL_CLIENT_ID)
                .unwrap_or_else(|| self.default_client_view_state(self.view.cols, self.view.rows));
            self.install_active_client_state(fallback);
            self.active_client_id = LOCAL_CLIENT_ID;
            self.client_identities.remove(&client_id);
            return;
        }

        self.capture_client_focus_profile(client_id);
        self.inactive_client_states.remove(&client_id);
        self.client_identities.remove(&client_id);
    }

    pub fn handle_key_event_for_client(
        &mut self,
        client_id: ClientId,
        key: KeyEvent,
    ) -> io::Result<AppSignal> {
        self.with_client_context(client_id, move |app| app.handle_key(key))
    }

    pub fn handle_action_for_client(
        &mut self,
        client_id: ClientId,
        action: CommandAction,
    ) -> AppSignal {
        self.with_client_context(client_id, move |app| {
            app.needs_render = true;
            app.handle_action(action)
        })
    }

    pub fn handle_paste_text_for_client(
        &mut self,
        client_id: ClientId,
        text: String,
    ) -> io::Result<AppSignal> {
        self.with_client_context(client_id, move |app| app.handle_paste(text))
    }

    pub fn handle_mouse_event_for_client(
        &mut self,
        client_id: ClientId,
        mouse: MouseEvent,
    ) -> io::Result<()> {
        self.with_client_context(client_id, move |app| {
            app.handle_mouse(mouse);
            Ok(())
        })
    }

    pub fn take_pending_clipboard_ansi_for_client(&mut self, client_id: ClientId) -> Vec<String> {
        if self.active_client_id == client_id {
            return std::mem::take(&mut self.view.pending_clipboard_ansi);
        }
        self.inactive_client_states
            .get_mut(&client_id)
            .map(|state| std::mem::take(&mut state.pending_clipboard_ansi))
            .unwrap_or_default()
    }

    pub fn take_pending_passthrough_ansi_for_client(&mut self, client_id: ClientId) -> Vec<String> {
        if self.active_client_id == client_id {
            return std::mem::take(&mut self.view.pending_passthrough_ansi);
        }
        self.inactive_client_states
            .get_mut(&client_id)
            .map(|state| std::mem::take(&mut state.pending_passthrough_ansi))
            .unwrap_or_default()
    }

    pub fn handle_client_resize_event(
        &mut self,
        client_id: ClientId,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        self.with_client_context(client_id, |app| {
            app.view.cols = cols;
            app.view.rows = rows;
            app.resize_sessions_to_max_client_viewport()?;
            app.needs_render = true;
            Ok(())
        })
    }

    pub fn apply_attach_target_for_client(
        &mut self,
        client_id: ClientId,
        target: &AttachTarget,
    ) -> Result<(), String> {
        self.with_client_context(client_id, |app| app.apply_attach_target(target))
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        self.handle_key_event_for_client(LOCAL_CLIENT_ID, key)
    }

    pub fn handle_paste_text(&mut self, text: String) -> io::Result<AppSignal> {
        self.handle_paste_text_for_client(LOCAL_CLIENT_ID, text)
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) -> io::Result<()> {
        self.handle_mouse_event_for_client(LOCAL_CLIENT_ID, mouse)
    }

    pub fn handle_resize_event(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.with_client_context(LOCAL_CLIENT_ID, |app| app.handle_resize(cols, rows))
    }

    fn kill_session_by_index(&mut self, session_index: usize) -> Result<bool, String> {
        if session_index >= self.sessions.len() {
            return Err("session index out of range".to_string());
        }

        if self.sessions.len() == 1 {
            let context = self.current_hook_context();
            self.write_log("killed final session; shutting down");
            self.emit_hook(HookEvent::SessionKilled, context);
            self.should_quit = true;
            self.needs_render = true;
            return Ok(true);
        }

        let removed = self.sessions.remove(session_index);
        let removed_context = HookContext {
            session_id: Some(removed.session_id.clone()),
            session_name: Some(removed.session.session_name().to_string()),
            ..HookContext::default()
        };
        if self.view.active_session == session_index {
            if session_index >= self.sessions.len() {
                self.view.active_session = self.sessions.len().saturating_sub(1);
            }
        } else if session_index < self.view.active_session {
            self.view.active_session -= 1;
        }
        self.view
            .pane_histories_by_session
            .remove(&removed.session_id);
        let max_session_index = self.sessions.len().saturating_sub(1);
        for state in self.inactive_client_states.values_mut() {
            if session_index < state.active_session {
                state.active_session -= 1;
            } else if state.active_session > max_session_index {
                state.active_session = max_session_index;
            }
            state.pane_histories_by_session.remove(&removed.session_id);
        }
        for profile in self.client_focus_profiles.values_mut() {
            profile
                .pane_histories_by_session
                .remove(&removed.session_id);
            if profile.active_session_id.as_deref() == Some(removed.session_id.as_str()) {
                profile.active_session_id = None;
            }
        }

        self.restore_focus_for_active_session_from_history();

        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::SessionKilled, removed_context);
        self.write_log(&format!("killed session {}", removed.session_id));
        Ok(false)
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        if self.current_session_mut().reset_focused_pane_view_scroll() {
            self.needs_render = true;
        }

        if self.view.locked_input {
            if (matches!(key.code, crossterm::event::KeyCode::Esc) && key.modifiers.is_empty())
                || self.view.keys.check_global_action(key) == Some(CommandAction::LeaveLockMode)
            {
                self.view.locked_input = false;
                self.set_message("lock mode off", Duration::from_secs(2));
                self.needs_render = true;
                return Ok(AppSignal::None);
            }
            return match encode_key_to_bytes(key) {
                Some(bytes) => self.handle_send_bytes(bytes),
                None => Ok(AppSignal::None),
            };
        }

        if !matches!(self.view.input_mode, InputMode::Normal) {
            self.needs_render = true;
            return self.handle_mode_key(key);
        }

        let prefix_active_before = self.view.keys.prefix_active();
        match self.view.keys.handle_key(key) {
            InputAction::Command(action) => {
                self.needs_render = true;
                Ok(self.handle_action(action))
            }
            InputAction::SendBytes(bytes) => {
                if self.view.keys.prefix_active() != prefix_active_before {
                    self.needs_render = true;
                }
                self.handle_send_bytes(bytes)
            }
            InputAction::Ignore => {
                if self.view.keys.prefix_active() != prefix_active_before {
                    self.needs_render = true;
                }
                Ok(AppSignal::None)
            }
        }
    }

    fn handle_send_bytes(&mut self, bytes: Vec<u8>) -> io::Result<AppSignal> {
        let ctrl_d = bytes.as_slice() == [0x04];
        match self.send_input_to_active_window(&bytes) {
            Ok(()) => {
                if ctrl_d && self.current_session_mut().focused_pane_closed() {
                    self.close_focused_or_quit("pane process exited");
                }
                Ok(AppSignal::None)
            }
            Err(err) if is_closed_pane_error(&err) => {
                self.close_focused_or_quit("write to closed pane");
                Ok(AppSignal::None)
            }
            Err(err) if ctrl_d => {
                self.set_message(
                    &format!("ctrl+d write failed: {err}"),
                    Duration::from_secs(3),
                );
                self.write_log(&format!("ctrl+d write failed: {err}"));
                Ok(AppSignal::None)
            }
            Err(err) => Err(err),
        }
    }

    fn send_input_to_active_window(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.current_session().active_window_synchronize_panes() {
            let _ = self.current_session_mut().send_to_active_window(bytes)?;
            Ok(())
        } else {
            self.current_session_mut().send_to_focused(bytes)
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !self.mouse_enabled || self.view.locked_input {
            self.view.mouse_drag = None;
            return;
        }

        // Handle scroll events in both Normal and cursor modes.
        const MOUSE_SCROLL_LINES: isize = 3;
        let pane_view_rows = usize::from(self.view.rows.saturating_sub(1)).max(1);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.needs_render = true;
                if let InputMode::CursorMode { ref mut state } = self.view.input_mode {
                    Self::cursor_mode_scroll_by(state, -MOUSE_SCROLL_LINES, pane_view_rows);
                } else {
                    self.current_session_mut()
                        .scroll_focused_pane(MOUSE_SCROLL_LINES, pane_view_rows);
                }
                return;
            }
            MouseEventKind::ScrollDown => {
                self.needs_render = true;
                if let InputMode::CursorMode { ref mut state } = self.view.input_mode {
                    Self::cursor_mode_scroll_by(state, MOUSE_SCROLL_LINES, pane_view_rows);
                } else {
                    self.current_session_mut()
                        .scroll_focused_pane(-MOUSE_SCROLL_LINES, pane_view_rows);
                }
                return;
            }
            _ => {}
        }

        if !matches!(self.view.input_mode, InputMode::Normal) {
            self.view.mouse_drag = None;
            return;
        }

        self.needs_render = true;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.view.mouse_drag = None;
                self.view.text_selection = None;
                let side_window_tree = self.side_window_tree_overlay();
                if let Some(side) = side_window_tree.as_ref()
                    && let Some(window_number) =
                        self.side_window_tree_window_number_at(side, mouse.column, mouse.row)
                {
                    if self
                        .current_session_mut()
                        .focus_window_number(window_number)
                        .is_ok()
                    {
                        self.record_focus_for_active_session();
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    return;
                }
                let frame =
                    self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
                if let Some(divider) = Self::mouse_divider_at(&frame, mouse.column, mouse.row)
                    && let Some(pane_id) = Self::mouse_anchor_pane_for_divider(&frame, divider)
                {
                    if self.current_session_mut().focus_pane_id(pane_id).is_ok() {
                        self.record_focus_for_active_session();
                        self.view.mouse_drag = Some(MouseDragState {
                            pane_id,
                            orientation: divider.orientation,
                            last_col: mouse.column,
                            last_row: mouse.row,
                        });
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    return;
                }

                if let Some(pane) = Self::mouse_pane_info_at(&frame, mouse.column, mouse.row) {
                    if self
                        .current_session_mut()
                        .focus_pane_id(pane.pane_id)
                        .is_ok()
                    {
                        self.record_focus_for_active_session();
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    let local_col = usize::from(mouse.column)
                        .saturating_sub(pane.rect.x)
                        .min(pane.rect.width.saturating_sub(1));
                    let local_row = usize::from(mouse.row)
                        .saturating_sub(pane.rect.y)
                        .min(pane.rect.height.saturating_sub(1));
                    let absolute_row = pane.view_row_origin.saturating_add(local_row);
                    self.view.text_selection = Some(TextSelectionState {
                        pane_id: pane.pane_id,
                        start_col: local_col,
                        start_abs_row: absolute_row,
                        end_col: local_col,
                        end_abs_row: absolute_row,
                        pane_x: pane.rect.x,
                        pane_y: pane.rect.y,
                        pane_width: pane.rect.width,
                        pane_height: pane.rect.height,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Text selection drag takes priority over divider drag
                if let Some(mut sel) = self.view.text_selection {
                    let side_window_tree = self.side_window_tree_overlay();
                    let frame =
                        self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
                    let Some(pane) = frame.panes.iter().find(|pane| pane.pane_id == sel.pane_id)
                    else {
                        self.view.text_selection = None;
                        return;
                    };
                    sel.pane_x = pane.rect.x;
                    sel.pane_y = pane.rect.y;
                    sel.pane_width = pane.rect.width;
                    sel.pane_height = pane.rect.height;
                    let col = usize::from(mouse.column)
                        .saturating_sub(sel.pane_x)
                        .min(sel.pane_width.saturating_sub(1));
                    let row = usize::from(mouse.row)
                        .saturating_sub(sel.pane_y)
                        .min(sel.pane_height.saturating_sub(1));
                    sel.end_col = col;
                    sel.end_abs_row = pane.view_row_origin.saturating_add(row);
                    self.view.text_selection = Some(sel);
                    return;
                }

                let Some(mut drag) = self.view.mouse_drag else {
                    return;
                };

                let delta_col = i32::from(mouse.column) - i32::from(drag.last_col);
                let delta_row = i32::from(mouse.row) - i32::from(drag.last_row);
                let (direction, amount) = match drag.orientation {
                    crate::ui::window_manager::DividerOrientation::Vertical => {
                        if delta_col == 0 {
                            return;
                        }
                        if delta_col > 0 {
                            (
                                crate::ui::window_manager::Direction::Right,
                                delta_col as u16,
                            )
                        } else {
                            (
                                crate::ui::window_manager::Direction::Left,
                                (-delta_col) as u16,
                            )
                        }
                    }
                    crate::ui::window_manager::DividerOrientation::Horizontal => {
                        if delta_row == 0 {
                            return;
                        }
                        if delta_row > 0 {
                            (crate::ui::window_manager::Direction::Down, delta_row as u16)
                        } else {
                            (
                                crate::ui::window_manager::Direction::Up,
                                (-delta_row) as u16,
                            )
                        }
                    }
                };

                let resized = {
                    let (cols, rows) = self.current_effective_pane_dims();
                    let session = self.current_session_mut();
                    if session.focus_pane_id(drag.pane_id).is_ok() {
                        session.resize_focused(direction, amount, cols, rows)
                    } else {
                        Err("mouse resize pane missing".to_string())
                    }
                };

                if resized.is_ok() {
                    drag.last_col = mouse.column;
                    drag.last_row = mouse.row;
                    self.view.mouse_drag = Some(drag);
                    self.needs_full_clear = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.view.mouse_drag = None;
                if let Some(sel) = self.view.text_selection.take()
                    && (sel.start_col != sel.end_col || sel.start_abs_row != sel.end_abs_row)
                {
                    self.copy_text_selection(&sel);
                }
            }
            _ => {}
        }
    }

    fn mouse_pane_info_at(
        frame: &crate::session::manager::RenderFrame,
        col: u16,
        row: u16,
    ) -> Option<&crate::session::manager::RenderPane> {
        let col = usize::from(col);
        let row = usize::from(row);
        frame.panes.iter().find(|pane| {
            let inside_x = col >= pane.rect.x && col < pane.rect.x + pane.rect.width;
            let inside_y = row >= pane.rect.y && row < pane.rect.y + pane.rect.height;
            inside_x && inside_y
        })
    }

    fn mouse_divider_at(
        frame: &crate::session::manager::RenderFrame,
        col: u16,
        row: u16,
    ) -> Option<crate::ui::window_manager::Divider> {
        let col = usize::from(col);
        let row = usize::from(row);
        frame
            .dividers
            .iter()
            .copied()
            .find(|divider| match divider.orientation {
                crate::ui::window_manager::DividerOrientation::Vertical => {
                    col == divider.x && row >= divider.y && row < divider.y + divider.len
                }
                crate::ui::window_manager::DividerOrientation::Horizontal => {
                    row == divider.y && col >= divider.x && col < divider.x + divider.len
                }
            })
    }

    fn mouse_anchor_pane_for_divider(
        frame: &crate::session::manager::RenderFrame,
        divider: crate::ui::window_manager::Divider,
    ) -> Option<usize> {
        match divider.orientation {
            crate::ui::window_manager::DividerOrientation::Vertical => frame
                .panes
                .iter()
                .find(|pane| pane.rect.x + pane.rect.width == divider.x)
                .or_else(|| frame.panes.iter().find(|pane| pane.rect.x == divider.x + 1))
                .map(|pane| pane.pane_id),
            crate::ui::window_manager::DividerOrientation::Horizontal => frame
                .panes
                .iter()
                .find(|pane| pane.rect.y + pane.rect.height == divider.y)
                .or_else(|| frame.panes.iter().find(|pane| pane.rect.y == divider.y + 1))
                .map(|pane| pane.pane_id),
        }
    }

    fn apply_selection_highlight(
        frame: &mut crate::session::manager::RenderFrame,
        sel: &TextSelectionState,
    ) {
        let Some(pane) = frame.panes.iter_mut().find(|p| p.pane_id == sel.pane_id) else {
            return;
        };
        if pane.rows.is_empty() {
            return;
        }

        // Normalize so start <= end
        let (start_abs_row, start_col, end_abs_row, end_col) = if sel.start_abs_row
            < sel.end_abs_row
            || (sel.start_abs_row == sel.end_abs_row && sel.start_col <= sel.end_col)
        {
            (
                sel.start_abs_row,
                sel.start_col,
                sel.end_abs_row,
                sel.end_col,
            )
        } else {
            (
                sel.end_abs_row,
                sel.end_col,
                sel.start_abs_row,
                sel.start_col,
            )
        };
        let visible_start = pane.view_row_origin;
        let visible_end = visible_start + pane.rows.len().saturating_sub(1);
        if end_abs_row < visible_start || start_abs_row > visible_end {
            return;
        }

        for abs_row in start_abs_row.max(visible_start)..=end_abs_row.min(visible_end) {
            let row = abs_row.saturating_sub(visible_start);
            let Some(cells) = pane.rows.get_mut(row) else {
                continue;
            };
            let from = if abs_row == start_abs_row {
                start_col
            } else {
                0
            };
            let to = if abs_row == end_abs_row {
                (end_col + 1).min(cells.len())
            } else {
                cells.len()
            };
            for cell in cells.get_mut(from..to).into_iter().flatten() {
                cell.style.reverse = !cell.style.reverse;
            }
        }
    }

    fn copy_text_selection(&mut self, sel: &TextSelectionState) {
        let text = {
            let session = self.current_session();
            let Some(total_lines) = session.pane_total_lines(sel.pane_id) else {
                return;
            };
            if total_lines == 0 {
                return;
            }

            let (mut start_abs_row, start_col, mut end_abs_row, end_col) = if sel.start_abs_row
                < sel.end_abs_row
                || (sel.start_abs_row == sel.end_abs_row && sel.start_col <= sel.end_col)
            {
                (
                    sel.start_abs_row,
                    sel.start_col,
                    sel.end_abs_row,
                    sel.end_col,
                )
            } else {
                (
                    sel.end_abs_row,
                    sel.end_col,
                    sel.start_abs_row,
                    sel.start_col,
                )
            };
            let last_row = total_lines.saturating_sub(1);
            start_abs_row = start_abs_row.min(last_row);
            end_abs_row = end_abs_row.min(last_row);

            let mut lines = Vec::new();
            for abs_row in start_abs_row..=end_abs_row {
                let cells = session
                    .pane_absolute_row_cells(sel.pane_id, abs_row)
                    .unwrap_or_default();
                let from = if abs_row == start_abs_row {
                    start_col
                } else {
                    0
                };
                let to = if abs_row == end_abs_row {
                    (end_col + 1).min(cells.len())
                } else {
                    cells.len()
                };
                let text: String = cells
                    .get(from..to)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|cell| cell.ch != '\0')
                    .map(|cell| cell.ch)
                    .collect();
                lines.push(text.trim_end().to_string());
            }
            lines.join("\n")
        };

        if text.trim().is_empty() {
            return;
        }
        match self.copy_text_for_active_client(&text) {
            Ok(()) => self.set_message("copied selection", Duration::from_secs(2)),
            Err(err) => self.set_message(&format!("copy failed: {err}"), Duration::from_secs(3)),
        }
    }

    fn copy_text_for_active_client(&mut self, text: &str) -> Result<(), String> {
        if self.active_client_id == LOCAL_CLIENT_ID {
            return crate::clipboard::copy_text(text);
        }
        self.view
            .pending_clipboard_ansi
            .push(crate::clipboard::osc52_sequence(text));
        Ok(())
    }

    fn close_focused_or_quit(&mut self, reason: &str) {
        let (cols, rows) = self.current_effective_pane_dims();

        if self.current_session().pane_count() <= 1 {
            if self.sessions.len() > 1 {
                let closed_session = self.current_session_id().to_string();
                match self.kill_session_by_index(self.view.active_session) {
                    Ok(false) => {
                        self.write_log(&format!(
                            "{reason}: final pane closed, switched from session {closed_session}"
                        ));
                        self.set_message("session closed", Duration::from_secs(2));
                    }
                    Ok(true) => {
                        self.write_log(&format!("{reason}: final pane closed, quitting"));
                        self.should_quit = true;
                    }
                    Err(err) => {
                        self.write_log(&format!(
                            "{reason}: failed to close session after pane exit: {err}"
                        ));
                        self.set_message(
                            &format!("pane close failed: {err}"),
                            Duration::from_secs(2),
                        );
                    }
                }
            } else {
                self.write_log(&format!("{reason}: final pane closed, quitting"));
                self.should_quit = true;
            }
            return;
        }

        if self.current_session_mut().close_focused(cols, rows).is_ok() {
            self.sync_tree_names();
            self.needs_full_clear = true;
            self.persist_active_session_info();
            self.emit_hook(HookEvent::PaneClosed, self.current_hook_context());
            self.write_log(&format!("{reason}: closed focused pane"));
            self.set_message("pane closed", Duration::from_secs(2));
        } else {
            self.set_message("pane close failed", Duration::from_secs(2));
        }
    }

    fn handle_mode_key(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        let mut signal = AppSignal::None;
        let mode = std::mem::replace(&mut self.view.input_mode, InputMode::Normal);
        self.view.input_mode = match mode {
            InputMode::RenameTreeItem { .. }
            | InputMode::ConfirmDelete { .. }
            | InputMode::SystemTree { .. } => self.handle_system_tree_mode_key(mode, key),
            InputMode::CursorMode { state } => self.handle_cursor_mode_key(state, key),
            InputMode::CommandPalette { state } => {
                self.handle_command_palette_mode_key(state, key, &mut signal)
            }
            InputMode::PeekAllWindows { state } => self.handle_peek_all_windows_mode_key(state),
            InputMode::Normal => InputMode::Normal,
        };
        Ok(signal)
    }

    fn open_peek_all_windows(&mut self) {
        let state = PeekAllWindowsState {
            session_id: self.current_session_id().to_string(),
            focused_window_number: self.current_session().focused_window_number(),
            focused_pane_id: self.current_session().focused_pane_id(),
        };
        self.view.input_mode = InputMode::PeekAllWindows { state };
        self.needs_full_clear = true;
    }

    fn handle_peek_all_windows_mode_key(&mut self, state: PeekAllWindowsState) -> InputMode {
        self.restore_peek_all_windows_focus(state);
        InputMode::Normal
    }

    fn restore_peek_all_windows_focus(&mut self, state: PeekAllWindowsState) {
        let Some(session_index) = self.session_index_for_id(&state.session_id) else {
            return;
        };

        self.view.active_session = session_index;
        let mut focused = false;

        if let Some(pane_id) = state.focused_pane_id {
            focused = self.current_session_mut().focus_pane_id(pane_id).is_ok();
        }

        if !focused && let Some(window_number) = state.focused_window_number {
            focused = self
                .current_session_mut()
                .focus_window_number(window_number)
                .is_ok();
        }

        if focused {
            self.record_focus_for_active_session();
        } else {
            self.restore_focus_for_active_session_from_history();
        }
        self.persist_active_session_info();
        self.needs_full_clear = true;
    }

    fn system_tree_overlay_for_state(
        &self,
        state: &SystemTreeState,
        rename: Option<(RenameTarget, &str)>,
    ) -> Option<crate::ui::render::SystemOverlay> {
        let rows = self.system_tree_rows(state);
        if rows.is_empty() {
            return None;
        }

        let candidates = self.system_tree_candidates(state, &rows);
        let selected_candidate = Self::selected_tree_candidate(state, &candidates);
        let selected = selected_candidate
            .as_ref()
            .map(|(index, _)| *index)
            .unwrap_or(0);
        let preview_lines = selected_candidate
            .and_then(|(_, candidate)| rows.get(candidate.row_index))
            .map(|row| self.tree_preview_lines(row))
            .unwrap_or_else(|| vec![TREE_PREVIEW_EMPTY.to_string()]);

        let mut query_active = state.query_active;
        let mut selected_cursor_pos = None;
        let mut candidate_labels = candidates
            .iter()
            .map(|candidate| rows[candidate.row_index].label.clone())
            .collect::<Vec<_>>();
        if let Some((target, buffer)) = rename {
            query_active = false;
            if let Some(selected_label) = candidate_labels.get_mut(selected) {
                let prefix = format!("rename {}: ", system_tree::rename_target_label(target));
                *selected_label = format!("{prefix}{buffer}");
                selected_cursor_pos = Some(prefix.chars().count() + buffer.chars().count());
            }
        }

        Some(crate::ui::render::SystemOverlay {
            title: "tree".to_string(),
            query: state.query_input.text.clone(),
            query_cursor_pos: state.query_input.cursor,
            query_active,
            candidates: candidate_labels,
            selected,
            selected_cursor_pos,
            preview_lines,
            preview_from_tail: true,
        })
    }

    fn system_overlay(&self) -> Option<crate::ui::render::SystemOverlay> {
        match &self.view.input_mode {
            InputMode::RenameTreeItem {
                target,
                buffer,
                return_tree: Some(state),
            } => self.system_tree_overlay_for_state(state, Some((*target, buffer))),
            InputMode::SystemTree { state } => self.system_tree_overlay_for_state(state, None),
            InputMode::CommandPalette { state } => {
                let entries = Self::command_palette_entries_for(self.view.locked_input);
                let recent_command_ids = self.command_history.get_recent_commands(100);
                let candidates =
                    Self::command_palette_candidates(state, &entries, &recent_command_ids);
                let selected = state.selected.min(candidates.len().saturating_sub(1));
                let preview_lines = candidates
                    .get(selected)
                    .map(|candidate| entries[candidate.entry_index].preview_lines.clone())
                    .unwrap_or_else(|| {
                        vec![
                            "no commands matched".to_string(),
                            "type to filter commands".to_string(),
                        ]
                    });

                Some(crate::ui::render::SystemOverlay {
                    title: "commands".to_string(),
                    query: state.text_input.text.clone(),
                    query_cursor_pos: state.text_input.cursor,
                    query_active: true,
                    candidates: candidates
                        .iter()
                        .map(|candidate| entries[candidate.entry_index].label.clone())
                        .collect(),
                    selected,
                    selected_cursor_pos: None,
                    preview_lines,
                    preview_from_tail: false,
                })
            }
            InputMode::ConfirmDelete { label, .. } => Some(crate::ui::render::SystemOverlay {
                title: "confirm delete".to_string(),
                query: String::new(),
                query_cursor_pos: 0,
                query_active: false,
                candidates: vec![
                    format!("Delete {label}?"),
                    "y = confirm, n/Esc = cancel".to_string(),
                ],
                selected: 0,
                selected_cursor_pos: None,
                preview_lines: vec![
                    format!("target: {label}"),
                    "y = delete, n = cancel".to_string(),
                ],
                preview_from_tail: false,
            }),
            _ => None,
        }
    }

    fn handle_paste(&mut self, text: String) -> io::Result<AppSignal> {
        self.needs_render = true;
        match &mut self.view.input_mode {
            InputMode::RenameTreeItem { buffer, .. } => {
                buffer.push_str(&text);
                Ok(AppSignal::None)
            }
            InputMode::SystemTree { .. } | InputMode::ConfirmDelete { .. } => Ok(AppSignal::None),
            InputMode::CursorMode { .. } => Ok(AppSignal::None),
            InputMode::CommandPalette { state } => {
                let text = text.trim_end_matches(['\r', '\n']).trim_end_matches('\0');
                if state.text_input.insert_text(text) {
                    let entries = Self::command_palette_entries_for(self.view.locked_input);
                    let recent_command_ids = self.command_history.get_recent_commands(100);
                    let candidates =
                        Self::command_palette_candidates(state, &entries, &recent_command_ids);
                    Self::command_palette_clamp_selected(state, candidates.len());
                }
                Ok(AppSignal::None)
            }
            InputMode::PeekAllWindows { .. } => Ok(AppSignal::None),
            InputMode::Normal => {
                self.send_input_to_active_window(text.as_bytes())?;
                Ok(AppSignal::None)
            }
        }
    }

    fn apply_action_effects(&mut self, effects: ActionEffects) {
        if effects.record_focus {
            self.record_focus_for_active_session();
        }
        if effects.sync_focus_history {
            self.sync_focus_history_for_active_session();
        }
        if effects.sync_tree_names {
            self.sync_tree_names();
        }
        if effects.full_clear {
            self.needs_full_clear = true;
        }
        if effects.persist_session_info {
            self.persist_active_session_info();
        }
        if effects.persist_runtime_state {
            self.persist_runtime_state();
        }
        if let Some(hook) = effects.hook {
            self.emit_hook(hook, self.current_hook_context());
        }
    }

    fn handle_action(&mut self, action: CommandAction) -> AppSignal {
        let (cols, rows) = self.current_effective_pane_dims();

        match action {
            CommandAction::Split(axis) => {
                if self
                    .current_session_mut()
                    .split_focused(axis, cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::structure(HookEvent::PaneSplit));
                }
            }
            CommandAction::Focus(direction) => {
                if self
                    .current_session_mut()
                    .focus(direction, cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::focus());
                } else {
                    match direction {
                        Direction::Left => {
                            return self.handle_action(CommandAction::PrevSession);
                        }
                        Direction::Right => {
                            return self.handle_action(CommandAction::NextSession);
                        }
                        Direction::Up => {
                            if self.current_session_mut().focus_prev_window().is_ok() {
                                self.apply_action_effects(ActionEffects::focus());
                            }
                        }
                        Direction::Down => {
                            if self.current_session_mut().focus_next_window().is_ok() {
                                self.apply_action_effects(ActionEffects::focus());
                            }
                        }
                    }
                }
            }
            CommandAction::FocusNextPane => {
                if self.focus_next_pane_history() {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::FocusPrevPane => {
                if self.focus_prev_pane_history() {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::ClosePane => {
                if self.current_session_mut().close_focused(cols, rows).is_ok() {
                    self.apply_action_effects(ActionEffects {
                        hook: Some(HookEvent::PaneClosed),
                        ..ActionEffects::reorder()
                    });
                }
            }
            CommandAction::Quit => self.should_quit = true,
            CommandAction::DetachClient => return AppSignal::DetachClient,

            CommandAction::SystemTree => self.open_system_tree(),
            CommandAction::SideWindowTree => self.toggle_side_window_tree(),
            CommandAction::PeekAllWindows => self.open_peek_all_windows(),
            CommandAction::EnterCursorMode => self.open_cursor_mode(),
            CommandAction::LeaveCursorMode => {
                if matches!(self.view.input_mode, InputMode::CursorMode { .. }) {
                    self.view.input_mode = InputMode::Normal;
                } else {
                    self.set_message("cursor mode is not active", Duration::from_secs(2));
                }
            }
            CommandAction::CommandPalette => self.open_command_palette(),
            CommandAction::NextWindow => {
                if self.current_session_mut().focus_next_window().is_ok() {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::PrevWindow => {
                if self.current_session_mut().focus_prev_window().is_ok() {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::SelectWindow(number) => {
                if self
                    .current_session_mut()
                    .focus_window_number(number)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::NewWindow => {
                if self.current_session_mut().new_window(cols, rows).is_ok() {
                    self.apply_action_effects(ActionEffects::structure(HookEvent::WindowCreated));
                }
            }

            CommandAction::Resize(direction) => {
                if self
                    .current_session_mut()
                    .resize_focused(direction, 5, cols, rows)
                    .is_ok()
                {
                    self.needs_full_clear = true;
                }
            }
            CommandAction::SwapPrevWindow => {
                if self.current_session_mut().swap_prev_window().is_ok() {
                    self.apply_action_effects(ActionEffects::reorder());
                }
            }
            CommandAction::SwapNextWindow => {
                if self.current_session_mut().swap_next_window().is_ok() {
                    self.apply_action_effects(ActionEffects::reorder());
                }
            }

            CommandAction::SaveLayout => self.save_active_layout(),
            CommandAction::WriteLog => self.write_log("manual log event"),
            CommandAction::WriteScrollback => self.write_active_scrollback(),
            CommandAction::OpenPaneBufferInEditor => self.open_current_pane_buffer_in_editor(),

            CommandAction::RenameSession => {
                let target = RenameTarget::Session {
                    session_index: self.view.active_session,
                };
                let buffer = self.rename_buffer_for_target(target);
                self.view.input_mode = InputMode::RenameTreeItem {
                    target,
                    buffer,
                    return_tree: None,
                };
            }
            CommandAction::NextSession => {
                if self.sessions.len() > 1 {
                    self.view.active_session = (self.view.active_session + 1) % self.sessions.len();
                    self.restore_focus_for_active_session_from_history();
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::PrevSession => {
                if self.sessions.len() > 1 {
                    if self.view.active_session == 0 {
                        self.view.active_session = self.sessions.len().saturating_sub(1);
                    } else {
                        self.view.active_session -= 1;
                    }
                    self.restore_focus_for_active_session_from_history();
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::NewSession => {
                self.create_session();
            }
            CommandAction::ToggleZoom => {
                if self
                    .current_session_mut()
                    .toggle_zoom_active_window(cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::ToggleSynchronizePanes => {
                if self
                    .current_session_mut()
                    .toggle_synchronize_panes_active_window()
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects {
                        persist_runtime_state: true,
                        ..Default::default()
                    });
                }
            }
            CommandAction::ReloadConfig => match self.reload_config_from_path(None) {
                Ok(message) => self.set_message(&message, Duration::from_secs(3)),
                Err(err) => self.set_message(&err, Duration::from_secs(3)),
            },
            CommandAction::CreateDefaultConfig => {
                let path = config::config_path();
                match self.create_default_config_at_path(&path) {
                    Ok(message) => self.set_message(&message, Duration::from_secs(3)),
                    Err(err) => self.set_message(&err, Duration::from_secs(3)),
                }
            }
            CommandAction::OpenConfigInEditor => self.open_config_in_editor(),
            CommandAction::EnterLockMode => {
                self.view.locked_input = true;
                self.set_message("lock mode on", Duration::from_secs(2));
            }
            CommandAction::LeaveLockMode => {
                self.view.locked_input = false;
                self.set_message("lock mode off", Duration::from_secs(2));
            }
            CommandAction::KillSession => {
                match self.kill_session_by_index(self.view.active_session) {
                    Ok(shutdown) => {
                        if shutdown {
                            self.set_message(
                                "killed final session; shutting down",
                                Duration::from_secs(2),
                            );
                        } else {
                            self.sync_tree_names();
                            self.needs_full_clear = true;
                            self.set_message("session killed", Duration::from_secs(2));
                        }
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("kill session failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            CommandAction::CloseWindow => {
                match self.current_session_mut().close_active_window(cols, rows) {
                    Ok(()) => {
                        self.apply_action_effects(ActionEffects::reorder());
                        self.set_message("window closed", Duration::from_secs(2));
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("close window failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
        }
        AppSignal::None
    }

    fn create_session(&mut self) {
        match self.create_session_internal() {
            Ok(_) => {
                self.set_message("session created", Duration::from_secs(2));
            }
            Err(err) => {
                self.set_message(
                    &format!("create session failed: {err}"),
                    Duration::from_secs(3),
                );
            }
        }
    }

    fn create_session_internal(&mut self) -> Result<String, String> {
        let ordinal = self.next_session_ordinal;
        self.next_session_ordinal += 1;

        let mut options = self.session_template.clone();
        options.session_name = format!("{}-{ordinal}", self.session_template.session_name);

        let (cols, rows) = self.current_effective_pane_dims();
        let mut session = SessionManager::new(options, cols, rows)
            .map_err(|err| format!("create session failed: {err}"))?;
        session
            .resize(cols, rows)
            .map_err(|err| format!("resize session failed: {err}"))?;

        let session_id = session_id_for(session.session_name(), ordinal);
        self.sessions.push(ManagedSession {
            ordinal,
            session_id: session_id.clone(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
        });
        self.view.active_session = self.sessions.len().saturating_sub(1);
        self.record_focus_for_active_session();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::SessionCreated, self.current_hook_context());
        self.write_log("created session");
        Ok(session_id)
    }

    fn reload_config_from_path(&mut self, path: Option<&str>) -> Result<String, String> {
        let path = path.map(PathBuf::from).unwrap_or_else(config::config_path);
        let loaded = config::load_from_path(Path::new(&path))
            .map_err(|err| format!("source-file failed: {err}"))?;
        self.apply_loaded_config(loaded);
        self.persist_runtime_state();
        self.emit_hook(HookEvent::ConfigReloaded, self.current_hook_context());
        let message = format!("config reloaded: {}", path.display());
        self.write_log(&message);
        Ok(message)
    }

    fn create_default_config_at_path(&mut self, path: &Path) -> Result<String, String> {
        let merged = if path.exists() {
            let contents = std::fs::read_to_string(path)
                .map_err(|err| format!("config read failed ({}): {err}", path.display()))?;
            toml::from_str::<config::AppConfig>(&contents)
                .map_err(|err| format!("config parse failed: {err}"))?
        } else {
            config::AppConfig::default()
        };

        let contents = toml::to_string_pretty(&merged)
            .map_err(|err| format!("config serialize failed: {err}"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "config directory create failed ({}): {err}",
                    parent.display()
                )
            })?;
        }

        std::fs::write(path, contents)
            .map_err(|err| format!("config write failed ({}): {err}", path.display()))?;

        self.apply_loaded_config(merged);
        self.persist_runtime_state();
        self.emit_hook(HookEvent::ConfigReloaded, self.current_hook_context());
        let message = format!("config written: {}", path.display());
        self.write_log(&message);
        Ok(message)
    }

    fn apply_loaded_config(&mut self, loaded: config::AppConfig) {
        let keys = KeyMapper::with_config(
            loaded.prefix.as_deref(),
            loaded.prefix_sticky,
            &loaded.prefix_bindings,
            &loaded.global_bindings,
        );
        self.key_template = keys.clone();
        self.view.keys = keys.clone();
        for state in self.inactive_client_states.values_mut() {
            state.keys = keys.clone();
        }
        self.status_format = loaded
            .status
            .format
            .clone()
            .unwrap_or_else(|| DEFAULT_STATUS_FORMAT.to_string());
        self.status_style = status_style_from_config(&loaded.status);
        self.hooks = loaded.hooks.clone();
        self.editor_command = normalize_editor_command(loaded.editor.clone());

        let suppress_prompt_eol_marker = loaded.shell.suppress_prompt_eol_marker;
        self.session_template.suppress_prompt_eol_marker = suppress_prompt_eol_marker;
        let allow_passthrough = loaded.terminal.allow_passthrough;
        self.session_template.allow_passthrough = allow_passthrough;
        for managed in &mut self.sessions {
            managed
                .session
                .set_suppress_prompt_eol_marker(suppress_prompt_eol_marker);
            managed.session.set_allow_passthrough(allow_passthrough);
        }

        self.mouse_enabled = loaded.mouse.enabled;
        if !self.mouse_enabled {
            self.view.mouse_drag = None;
        }
    }

    fn status_line(&self) -> String {
        match &self.view.input_mode {
            InputMode::RenameTreeItem {
                target,
                return_tree: Some(_),
                ..
            } => {
                return format!(
                    "tree popup (rename {}): type name, Enter save, Backspace delete, Esc cancel",
                    system_tree::rename_target_label(*target)
                );
            }
            InputMode::RenameTreeItem {
                target,
                buffer,
                return_tree: None,
            } => {
                return format!(
                    "rename {}: {buffer} (Enter save, Esc cancel)",
                    system_tree::rename_target_label(*target)
                );
            }
            InputMode::SystemTree { state } => {
                let mode = if state.query_active {
                    "query"
                } else {
                    "candidates"
                };
                return format!(
                    "tree popup ({mode}): / query focus, query keys Left/Right Ctrl+f/b/a/e Ctrl+Left/Right Ctrl+w/k/u, Down or Ctrl+n/p/j enter candidates, candidate keys Up/Down Left/Right collapse-expand, Up on first returns query, Enter select, r rename, Backspace delete, Esc cancel"
                );
            }
            InputMode::ConfirmDelete { label, .. } => {
                return format!("Delete {label}? (y/n, Esc cancel)");
            }
            InputMode::CursorMode { .. } => {
                return "cursor mode: h/j/k/l or arrows move (clear anchor), w/b/e word (set anchor), 0/$ line start/end (clear anchor), v toggle anchor, x linewise select/extend, y copy, Esc/q exit".to_string();
            }
            InputMode::CommandPalette { .. } => {
                return "command palette: type filter, Left/Right edit, Up/Down select, Enter run, Ctrl+n/p/j nav, Ctrl+f/b/a/e move, Ctrl+Left/Right word, Ctrl+w/k delete, Ctrl+c/q or Esc cancel".to_string();
            }
            InputMode::PeekAllWindows { .. } => {
                return "peek all panes: any key exits and restores focus".to_string();
            }
            InputMode::Normal => {}
        }

        let session = self.current_session();
        let prefix_state = if self.view.keys.prefix_active() {
            "on"
        } else {
            "off"
        };
        let pane_index = session
            .focused_window_number()
            .and_then(|window_number| {
                let pane_id = session.focused_pane_id()?;
                let pane_ids = session.pane_ids_for_window_number(window_number)?;
                pane_ids
                    .iter()
                    .position(|current| *current == pane_id)
                    .map(|index| index + 1)
            })
            .unwrap_or(0);
        let mut line = self.status_format.clone();
        for (token, value) in [
            (
                "{session_index}",
                (self.view.active_session + 1).to_string(),
            ),
            ("{session_count}", self.sessions.len().to_string()),
            ("{session_id}", self.current_session_id().to_string()),
            ("{session_name}", session.session_name().to_string()),
            (
                "{window_index}",
                session.focused_window_number().unwrap_or(0).to_string(),
            ),
            ("{window_count}", session.window_count().to_string()),
            (
                "{window_id}",
                session.focused_window_id().unwrap_or(0).to_string(),
            ),
            (
                "{pane_id}",
                session.focused_pane_id().unwrap_or(0).to_string(),
            ),
            ("{pane_index}", pane_index.to_string()),
            ("{pane_count}", session.pane_count().to_string()),
            ("{prefix}", prefix_state.to_string()),
            (
                "{lock}",
                if self.view.locked_input {
                    " | LOCK".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "{zoom}",
                if session.active_window_zoomed() {
                    " | ZOOM".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "{sync}",
                if session.active_window_synchronize_panes() {
                    " | SYNC".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "{mouse}",
                if self.mouse_enabled {
                    " | MOUSE".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "{message}",
                self.view
                    .status_message
                    .as_ref()
                    .map(|message| format!(" | {}", message.text))
                    .unwrap_or_default(),
            ),
        ] {
            line = line.replace(token, &value);
        }
        line
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
