use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crossterm::style::Color;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::input::{CommandAction, KeyMapper};
use crate::session::manager::SessionManager;
use crate::session::terminal_state::{CellStyle, StyledCell};
use crate::ui::window_manager::WindowId;
use crate::input::text_input::TextInput;

pub(super) struct ManagedSession {
    pub ordinal: usize,
    pub session_id: String,
    pub session: SessionManager,
    pub window_names: HashMap<WindowId, String>,
    pub pane_names: HashMap<usize, String>,
    pub window_auto_names: HashMap<WindowId, String>,
    pub pane_auto_names: HashMap<usize, String>,
    pub terminal_titles: HashMap<usize, String>,
    pub cwd_fallbacks: HashMap<usize, String>,
}

pub(super) enum InputMode {
    Normal,
    RenameTreeItem {
        target: RenameTarget,
        buffer: String,
        return_tree: Option<SystemTreeState>,
    },
    SystemTree {
        state: SystemTreeState,
    },
    ConfirmDelete {
        target: TreeRowKind,
        label: String,
        return_tree: SystemTreeState,
    },
    CursorMode {
        state: CursorModeState,
    },
    CommandPalette {
        state: CommandPaletteState,
    },
    PeekAllWindows {
        state: PeekAllWindowsState,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) enum RenameTarget {
    Session {
        session_index: usize,
    },
    Window {
        session_index: usize,
        window_id: WindowId,
    },
    Pane {
        session_index: usize,
        pane_id: usize,
    },
}

#[derive(Debug, Clone, Default)]
pub(super) struct SystemTreeState {
    pub cursor_row: usize,
    pub expanded_sessions: HashSet<usize>,
    pub expanded_windows: HashSet<TreeWindowKey>,
    pub query_input: TextInput,
    pub query_active: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CursorModeState {
    pub pane_id: usize,
    pub lines: Vec<String>,
    pub styled_lines: Vec<Vec<StyledCell>>,
    pub cursor: CursorModePoint,
    pub selection_anchor: Option<CursorModePoint>,
    pub viewport_top: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct CursorModePoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CommandPaletteState {
    pub text_input: TextInput,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub(super) struct PeekAllWindowsState {
    pub session_id: String,
    pub focused_window_number: Option<usize>,
    pub focused_pane_id: Option<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct CommandPaletteEntry {
    pub id: String,
    pub action: CommandAction,
    pub label: String,
    pub search_key: String,
    pub preview_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScoredCommandCandidate {
    pub entry_index: usize,
    pub score: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct TreeWindowKey {
    pub session_index: usize,
    pub window_index: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum TreeRowKind {
    Session {
        session_index: usize,
    },
    Window {
        session_index: usize,
        window_index: usize,
        window_number: usize,
        window_id: WindowId,
    },
    Pane {
        session_index: usize,
        pane_id: usize,
    },
}

#[derive(Debug, Clone)]
pub(super) struct TreeRow {
    pub kind: TreeRowKind,
    pub parent_row: Option<usize>,
    pub has_children: bool,
    pub expanded: bool,
    pub label: String,
}

#[derive(Debug, Clone)]
pub(super) struct ScoredTreeCandidate {
    pub row_index: usize,
    pub score: i32,
}

pub(super) struct TimedMessage {
    pub text: String,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct MouseDragState {
    pub pane_id: usize,
    pub orientation: crate::ui::window_manager::DividerOrientation,
    pub last_col: u16,
    pub last_row: u16,
}

/// Mouse text selection state for drag-to-select-and-copy.
#[derive(Debug, Clone, Copy)]
pub(super) struct TextSelectionState {
    pub pane_id: usize,
    /// Pane-local column where the selection started.
    pub start_col: usize,
    /// Absolute buffer row where the selection started.
    pub start_abs_row: usize,
    /// Pane-local column of the current selection end.
    pub end_col: usize,
    /// Absolute buffer row of the current selection end.
    pub end_abs_row: usize,
    /// Pane position in terminal coordinates (for coord conversion on drag).
    pub pane_x: usize,
    pub pane_y: usize,
    pub pane_width: usize,
    pub pane_height: usize,
}

pub(super) const RUNTIME_STATE_VERSION: u8 = 1;
pub(super) const DEFAULT_STATUS_FORMAT: &str = "session {session_index}/{session_count}:{session_name} | window {window_index}/{window_count} | pane {pane_index}/{pane_count} | prefix {prefix}{lock}{zoom}{sync}{mouse}{message}";
pub(super) const DEFAULT_STATUS_BG: Color = Color::Rgb {
    r: 0x2E,
    g: 0x34,
    b: 0x40,
};
pub(super) const DEFAULT_STATUS_FG: Color = Color::Rgb {
    r: 0xD8,
    g: 0xDE,
    b: 0xE9,
};
pub(super) const TREE_PREVIEW_MAX_LINES: usize = 400;
pub(super) const TREE_PREVIEW_EMPTY: &str = "no pane output";
pub(super) const LOCAL_CLIENT_FOCUS_IDENTITY: &str = "local";

#[derive(Debug, Clone, Copy)]
pub(super) enum HookEvent {
    SessionCreated,
    SessionKilled,
    WindowCreated,
    PaneSplit,
    PaneClosed,
    ConfigReloaded,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session_created",
            Self::SessionKilled => "session_killed",
            Self::WindowCreated => "window_created",
            Self::PaneSplit => "pane_split",
            Self::PaneClosed => "pane_closed",
            Self::ConfigReloaded => "config_reloaded",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct HookContext {
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub window_id: Option<WindowId>,
    pub window_number: Option<usize>,
    pub pane_id: Option<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeUiConfig {
    pub keys: KeyMapper,
    pub mouse_enabled: bool,
    pub status_format: String,
    pub status_style: CellStyle,
    pub hooks: config::HooksConfig,
    pub editor_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorPaneCloseTarget {
    pub session_id: String,
    pub pane_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AppRuntimeState {
    pub version: u8,
    pub active_session: usize,
    pub next_session_ordinal: usize,
    pub sessions: Vec<SessionRuntimeState>,
    #[serde(default)]
    pub client_focus_profiles: HashMap<String, PersistedClientFocusState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SessionRuntimeState {
    pub ordinal: usize,
    pub session_id: String,
    pub session: crate::session::manager::SessionRuntimeSnapshot,
    pub window_names: HashMap<WindowId, String>,
    pub pane_names: HashMap<usize, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct PaneFocusHistorySnapshot {
    #[serde(default)]
    pub pane_ids: Vec<usize>,
    pub index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct PersistedClientFocusState {
    pub active_session_id: Option<String>,
    #[serde(default)]
    pub pane_histories_by_session: HashMap<String, PaneFocusHistorySnapshot>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct PaneFocusHistory {
    pub pane_ids: Vec<usize>,
    pub index: Option<usize>,
}

impl PaneFocusHistory {
    pub fn from_snapshot(snapshot: PaneFocusHistorySnapshot) -> Self {
        let mut history = Self {
            pane_ids: snapshot.pane_ids,
            index: snapshot.index,
        };
        history.clamp_index();
        history
    }

    pub fn snapshot(&self) -> PaneFocusHistorySnapshot {
        PaneFocusHistorySnapshot {
            pane_ids: self.pane_ids.clone(),
            index: self.index,
        }
    }

    pub fn record_focus(&mut self, pane_id: usize) {
        if let Some(pos) = self.pane_ids.iter().position(|id| *id == pane_id) {
            self.pane_ids.remove(pos);
        }
        self.pane_ids.push(pane_id);
        self.index = Some(self.pane_ids.len().saturating_sub(1));
    }

    pub fn sync_index_from_current(&mut self, pane_id: usize) {
        if let Some(pos) = self.pane_ids.iter().position(|id| *id == pane_id) {
            self.index = Some(pos);
        } else {
            self.record_focus(pane_id);
        }
    }

    pub fn current_pane(&self) -> Option<usize> {
        self.index.and_then(|idx| self.pane_ids.get(idx).copied())
    }

    pub fn prev_from(&mut self, current_pane_id: usize) -> Option<usize> {
        if self.pane_ids.is_empty() {
            self.index = None;
            return None;
        }

        let current_idx = match self.pane_ids.iter().position(|id| *id == current_pane_id) {
            Some(0) => {
                self.index = Some(0);
                return None;
            }
            Some(idx) => idx,
            None => self.pane_ids.len().saturating_sub(1),
        };
        let target_idx = current_idx.saturating_sub(1);
        self.index = Some(target_idx);
        self.pane_ids.get(target_idx).copied()
    }

    pub fn next_from(&mut self, current_pane_id: usize) -> Option<usize> {
        if self.pane_ids.is_empty() {
            self.index = None;
            return None;
        }

        let current_idx = self.pane_ids.iter().position(|id| *id == current_pane_id)?;
        if current_idx + 1 >= self.pane_ids.len() {
            self.index = Some(current_idx);
            return None;
        }
        let target_idx = current_idx + 1;
        self.index = Some(target_idx);
        self.pane_ids.get(target_idx).copied()
    }

    pub fn prune_invalid(&mut self, valid_pane_ids: &HashSet<usize>) {
        self.pane_ids
            .retain(|pane_id| valid_pane_ids.contains(pane_id));
        self.clamp_index();
    }

    pub fn is_empty(&self) -> bool {
        self.pane_ids.is_empty()
    }

    fn clamp_index(&mut self) {
        self.index = match (self.index, self.pane_ids.len()) {
            (_, 0) => None,
            (Some(idx), len) if idx < len => Some(idx),
            (_, len) => Some(len - 1),
        };
    }
}

#[derive(Default)]
pub(super) struct ActionEffects {
    pub record_focus: bool,
    pub sync_focus_history: bool,
    pub sync_tree_names: bool,
    pub full_clear: bool,
    pub persist_session_info: bool,
    pub persist_runtime_state: bool,
    pub hook: Option<HookEvent>,
}

impl ActionEffects {
    pub fn focus() -> Self {
        Self {
            record_focus: true,
            persist_session_info: true,
            ..Default::default()
        }
    }

    pub fn structure(hook: HookEvent) -> Self {
        Self {
            record_focus: true,
            sync_tree_names: true,
            full_clear: true,
            persist_session_info: true,
            hook: Some(hook),
            ..Default::default()
        }
    }

    pub fn reorder() -> Self {
        Self {
            sync_focus_history: true,
            sync_tree_names: true,
            full_clear: true,
            persist_session_info: true,
            ..Default::default()
        }
    }

    pub fn layout() -> Self {
        Self {
            full_clear: true,
            persist_session_info: true,
            ..Default::default()
        }
    }
}

pub(super) struct ClientViewState {
    pub keys: KeyMapper,
    pub input_mode: InputMode,
    pub status_message: Option<TimedMessage>,
    pub locked_input: bool,
    pub mouse_drag: Option<MouseDragState>,
    pub text_selection: Option<TextSelectionState>,
    pub pending_clipboard_ansi: Vec<String>,
    pub pending_passthrough_ansi: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    pub active_session: usize,
    pub pane_histories_by_session: HashMap<String, PaneFocusHistory>,
    pub side_window_tree_open: bool,
}
