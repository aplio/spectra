use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::agent::AgentStatus;

use crossterm::style::Color;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::input::text_input::TextInput;
use crate::input::{CommandAction, KeyMapper};
use crate::session::manager::SessionManager;
use crate::session::terminal_state::{CellStyle, StyledCell};
use crate::ui::window_manager::WindowId;

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
    pub agents: AgentTracking,
}

/// Per-session AI-agent detection state, keyed by pane id.
#[derive(Debug, Default)]
pub(super) struct AgentTracking {
    /// Latest detection result per pane; absent = no agent detected.
    pub statuses: HashMap<usize, AgentStatus>,
    /// When detection last ran per pane (throttle bookkeeping).
    pub last_run: HashMap<usize, Instant>,
    /// Panes whose output changed but whose detection was throttled.
    pub pending: HashSet<usize>,
    /// Panes viewed since their last agent state change. An idle agent whose
    /// pane is not in this set displays as "done" (derived, never stored).
    pub seen: HashSet<usize>,
    /// Last state each pane raised a host notification for (debounce); an
    /// entry is removed when the pane leaves notifiable states, re-arming it.
    pub notified: HashMap<usize, crate::agent::AgentDisplayState>,
    /// When each pane last received an external `agent.report`. While the
    /// report is fresh (< [`REPORTED_AGENT_TTL`]) manifest detection is
    /// suppressed for that pane, so the reported status stays authoritative;
    /// afterwards detection resumes and overwrites it.
    pub reported: HashMap<usize, Instant>,
}

impl AgentTracking {
    /// Drop bookkeeping for panes that no longer exist. Returns true when a
    /// visible status entry was removed.
    pub fn prune_closed_panes(&mut self, pane_exists: impl Fn(usize) -> bool) -> bool {
        let before = self.statuses.len();
        self.statuses.retain(|pane_id, _| pane_exists(*pane_id));
        self.last_run.retain(|pane_id, _| pane_exists(*pane_id));
        self.pending.retain(|pane_id| pane_exists(*pane_id));
        self.seen.retain(|pane_id| pane_exists(*pane_id));
        self.notified.retain(|pane_id, _| pane_exists(*pane_id));
        self.reported.retain(|pane_id, _| pane_exists(*pane_id));
        self.statuses.len() != before
    }

    /// Whether an external `agent.report` for this pane is still within its
    /// validity window, i.e. manifest detection must stay suppressed.
    pub fn report_fresh(&self, pane_id: usize, now: Instant) -> bool {
        self.reported
            .get(&pane_id)
            .is_some_and(|reported_at| now.duration_since(*reported_at) < REPORTED_AGENT_TTL)
    }

    /// Display state for one pane's agent, deriving "done" from the seen
    /// flag; `None` when no agent is detected in the pane.
    pub fn display_state(&self, pane_id: usize) -> Option<crate::agent::AgentDisplayState> {
        let status = self.statuses.get(&pane_id)?;
        Some(status.display_state(self.seen.contains(&pane_id)))
    }

    /// Update the seen flag after a pane's stored agent status changed.
    ///
    /// Herdr-style "done": only a Working/Blocked → Idle transition that
    /// happens while the pane is not being viewed marks the result unseen;
    /// every other change (including any transition on a viewed pane) counts
    /// as seen, so a pane the user is looking at never shows "done".
    pub fn note_status_change(
        &mut self,
        pane_id: usize,
        previous: Option<crate::agent::AgentState>,
        next: crate::agent::AgentState,
        viewing: bool,
    ) {
        use crate::agent::AgentState;
        let finished_unwatched = next == AgentState::Idle
            && matches!(
                previous,
                Some(AgentState::Working) | Some(AgentState::Blocked)
            )
            && !viewing;
        if finished_unwatched {
            self.seen.remove(&pane_id);
        } else {
            self.seen.insert(pane_id);
        }
    }

    /// Decide whether a stored agent state change should raise a
    /// host-terminal notification, updating the per-pane debounce.
    ///
    /// Notifiable states are `Blocked` (modes "blocked" and "all") and the
    /// derived `Done` — a Working/Blocked → Idle transition on an unviewed
    /// pane (mode "all" only). The pane the user is looking at never
    /// notifies, and a pane re-notifies for a state only after its stored
    /// state moved away and back.
    pub fn notifiable_transition(
        &mut self,
        pane_id: usize,
        previous: Option<crate::agent::AgentState>,
        next: crate::agent::AgentState,
        viewing: bool,
        mode: config::AgentNotifyMode,
    ) -> Option<crate::agent::AgentDisplayState> {
        use crate::agent::{AgentDisplayState, AgentState};
        let display = match next {
            AgentState::Blocked => Some(AgentDisplayState::Blocked),
            AgentState::Idle
                if matches!(
                    previous,
                    Some(AgentState::Working) | Some(AgentState::Blocked)
                ) && !viewing =>
            {
                Some(AgentDisplayState::Done)
            }
            _ => None,
        };
        let Some(display) = display else {
            self.notified.remove(&pane_id);
            return None;
        };
        if viewing || self.notified.get(&pane_id) == Some(&display) {
            return None;
        }
        let wanted = match mode {
            config::AgentNotifyMode::Off => false,
            config::AgentNotifyMode::Blocked => display == AgentDisplayState::Blocked,
            config::AgentNotifyMode::All => true,
        };
        if !wanted {
            return None;
        }
        self.notified.insert(pane_id, display);
        Some(display)
    }
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
    Keybindings {
        state: KeybindingsState,
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
    /// True while a `v` visual selection is active: movement then extends
    /// the selection instead of dropping the anchor.
    pub visual: bool,
    pub viewport_top: usize,
    /// True after a bare `g` press, waiting for the second key of a `g`
    /// chord (`gg`, `ge`, `gh`, `gl`). Reset on the next key.
    pub pending_goto: bool,
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

/// App-state snapshot used to decide which command-palette entries are
/// applicable; extend it when future entries need more context.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CommandPaletteContext {
    pub locked_input: bool,
    pub cursor_mode_active: bool,
    /// Whether a recently closed pane is still retained for undo close.
    pub can_restore_pane: bool,
}

/// One row of the keyboard-shortcut cheat sheet: a key display and what it does.
#[derive(Debug, Clone)]
pub(super) struct KeybindingRow {
    pub keys: String,
    pub description: String,
}

/// State for the `prefix + ?` keyboard-shortcut cheat sheet overlay.
///
/// `rows` is the full binding list captured when the overlay opens. Navigation
/// is the default; pressing `/` activates the filter, mirroring the tree popup.
/// `selected` indexes the currently visible (filtered) candidate list.
#[derive(Debug, Clone, Default)]
pub(super) struct KeybindingsState {
    pub rows: Vec<KeybindingRow>,
    pub query_input: TextInput,
    pub query_active: bool,
    pub selected: usize,
}

/// One row of the side window tree, which spans every session. A row is either
/// a session header or a window belonging to that session. Both rendering and
/// click hit-testing derive from the same ordered row list so their geometry
/// can never drift.
#[derive(Debug, Clone)]
pub(super) struct SideTreeLayoutRow {
    pub session_index: usize,
    /// `None` marks a session header row; `Some(..)` a window under it.
    pub window: Option<SideTreeWindowRow>,
}

#[derive(Debug, Clone)]
pub(super) struct SideTreeWindowRow {
    pub window_number: usize,
    pub window_id: WindowId,
    pub pane_ids: Vec<usize>,
    /// Whether this window is the focused window within its own session.
    pub focused: bool,
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

/// Mouse text selection state for drag-to-select.
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
    /// True while the left button is held; a completed selection stays
    /// visible with `dragging: false` until a key press or the next click.
    pub dragging: bool,
}

/// Rapid-click chain for smart multi-click selection (word/line expansion).
/// Mirrors gargo's expand chain: successive clicks near the same cell within
/// the multi-click window grow the selection to the next larger unit.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClickChainState {
    pub pane_id: usize,
    /// Pane-local column of the first click in the chain. Expansion always
    /// derives candidates from this origin, so mouse jitter doesn't drift it.
    pub origin_col: usize,
    /// Absolute buffer row of the first click in the chain.
    pub origin_abs_row: usize,
    /// Column range (inclusive) selected by the most recent expand step;
    /// `None` until the second click promotes the chain to a word selection.
    pub last_range: Option<(usize, usize)>,
    pub last_click_time: Instant,
    /// Pane-local column of the most recent click, for the proximity check.
    pub last_col: usize,
}

pub(super) const RUNTIME_STATE_VERSION: u8 = 1;
pub(super) const DEFAULT_STATUS_FORMAT: &str = "session {session_index}/{session_count}:{session_name} | window {window_index}/{window_count} | pane {pane_index}/{pane_count} | prefix {prefix}{lock}{zoom}{sync}{mouse}{message}";
pub(super) const DEFAULT_SIDEBAR_SESSION_FORMAT: &str = "{session_name}";
pub(super) const DEFAULT_SIDEBAR_WINDOW_FORMAT: &str = "{window_label}";
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
/// Minimum interval between agent-detection runs for one pane.
pub(super) const AGENT_DETECT_INTERVAL: Duration = Duration::from_millis(200);
/// Validity window of an external `agent.report`: manifest detection is
/// suppressed for the reported pane until this much time has passed since
/// the last report, after which detection resumes.
pub(super) const REPORTED_AGENT_TTL: Duration = Duration::from_secs(30);
/// Maximum queued API events awaiting fan-out; the oldest is dropped beyond.
pub(super) const API_EVENT_QUEUE_MAX: usize = 1024;
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
    PaneRestored,
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
            Self::PaneRestored => "pane_restored",
            Self::ConfigReloaded => "config_reloaded",
        }
    }

    /// Name of the JSON-RPC API event bridged from this hook emission.
    pub fn api_event_name(self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::SessionKilled => "session.killed",
            Self::WindowCreated => "window.created",
            Self::PaneSplit => "pane.split",
            Self::PaneClosed => "pane.closed",
            Self::PaneRestored => "pane.restored",
            Self::ConfigReloaded => "config.reloaded",
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

impl HookContext {
    /// JSON params of the API event bridged from this hook emission
    /// (same context fields the hook receives via `SPECTRA_*` env vars).
    pub fn api_event_params(&self) -> serde_json::Value {
        serde_json::json!({
            "session_id": self.session_id,
            "session_name": self.session_name,
            "window_id": self.window_id,
            "window_number": self.window_number,
            "pane_id": self.pane_id,
        })
    }
}

/// Resolved `[sidebar]` row formats: `{token}` strings where `\n` splits an
/// entry into multiple sidebar lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SidebarFormats {
    pub session: String,
    pub window: String,
}

impl Default for SidebarFormats {
    fn default() -> Self {
        Self {
            session: DEFAULT_SIDEBAR_SESSION_FORMAT.to_string(),
            window: DEFAULT_SIDEBAR_WINDOW_FORMAT.to_string(),
        }
    }
}

impl SidebarFormats {
    pub fn from_config(sidebar: &config::SidebarConfig) -> Self {
        Self {
            session: sidebar
                .session_format
                .clone()
                .unwrap_or_else(|| DEFAULT_SIDEBAR_SESSION_FORMAT.to_string()),
            window: sidebar
                .window_format
                .clone()
                .unwrap_or_else(|| DEFAULT_SIDEBAR_WINDOW_FORMAT.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeUiConfig {
    pub keys: KeyMapper,
    pub mouse_enabled: bool,
    pub status_format: String,
    pub status_style: CellStyle,
    pub hooks: config::HooksConfig,
    pub editor_command: Option<String>,
    pub agent_notify: config::AgentNotifyMode,
    /// Whether the window-tree sidebar starts open on session start.
    pub sidebar_default_open: bool,
    pub sidebar_formats: SidebarFormats,
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
    pub click_chain: Option<ClickChainState>,
    pub pending_clipboard_ansi: Vec<String>,
    pub pending_passthrough_ansi: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    pub active_session: usize,
    pub pane_histories_by_session: HashMap<String, PaneFocusHistory>,
    pub side_window_tree_open: bool,
}
