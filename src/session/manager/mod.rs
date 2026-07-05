use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::io::host_colors::HostColors;
use crate::session::pane::Pane;
use crate::session::pty_backend::{PaneFactory, PaneSpawnConfig, PtyPaneFactory};
use crate::session::terminal_state::{StyledCell, TerminalEvent};
use crate::ui::window_manager::{
    Direction, Divider, Layout, PaneId, PaneLayout, PaneRect, SplitAxis, WindowId, WindowManager,
    WindowManagerSnapshot,
};

mod persistence;
mod render;
mod windows;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub shell: String,
    pub cwd: Option<PathBuf>,
    pub command: Vec<String>,
    pub session_name: String,
    /// API-level session id (normalized name + ordinal, e.g. "main-1") set
    /// by the App layer; exported to panes as SPECTRA_SESSION_ID.
    pub session_id: String,
    pub suppress_prompt_eol_marker: bool,
    pub allow_passthrough: bool,
    /// Host terminal default fg/bg colors applied to new panes so guests
    /// can query them via OSC 10/11 (unknown by default).
    pub host_colors: HostColors,
}

impl SessionOptions {
    pub fn from_cli(shell: Option<String>, cwd: Option<PathBuf>, command: Vec<String>) -> Self {
        Self {
            shell: shell.unwrap_or_else(default_shell),
            cwd,
            command,
            session_name: "main".to_string(),
            session_id: String::new(),
            suppress_prompt_eol_marker: false,
            allow_passthrough: true,
            host_colors: HostColors::default(),
        }
    }

    pub fn with_session_name(mut self, session_name: impl Into<String>) -> Self {
        self.session_name = session_name.into();
        self
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

#[derive(Debug, Clone)]
pub struct RenderPane {
    pub pane_id: PaneId,
    pub rect: PaneRect,
    pub view_row_origin: usize,
    pub rows: Vec<Vec<StyledCell>>,
    pub cursor: (usize, usize),
    pub focused: bool,
}

#[derive(Debug, Clone)]
pub struct RenderFrame {
    pub panes: Vec<RenderPane>,
    pub dividers: Vec<Divider>,
    pub focused_cursor: Option<(u16, u16)>,
    pub cursor_style: crossterm::cursor::SetCursorStyle,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowEntry {
    pub index: usize,
    pub window_id: WindowId,
    pub pane_id: PaneId,
    pub pane_ids: Vec<PaneId>,
    pub focused: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTerminalEvent {
    pub pane_id: PaneId,
    pub event: TerminalEvent,
}

/// One pane's transferable state, exported by the outgoing server during a
/// live handoff. The fd is borrowed (still owned by the pane backend); the
/// handoff duplicates it before sending.
#[cfg(unix)]
pub struct PaneHandoffExport {
    pub master_fd: std::os::fd::RawFd,
    pub child_pid: Option<u32>,
    pub replay: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedLayout {
    pub session_name: String,
    pub focused_window_number: Option<usize>,
    pub focused_pane_id: Option<PaneId>,
    pub windows: Vec<SavedWindowLayout>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedWindowLayout {
    pub index: usize,
    pub window_id: WindowId,
    pub focused: bool,
    pub focused_pane_id: Option<PaneId>,
    pub panes: Vec<SavedPaneLayout>,
    pub dividers: Vec<Divider>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedPaneLayout {
    pub pane_id: PaneId,
    pub rect: PaneRect,
    pub focused: bool,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRuntimeSnapshot {
    pub session_name: String,
    pub next_pane_id: PaneId,
    pub next_window_id: WindowId,
    pub active_window: usize,
    pub windows: Vec<SessionWindowSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWindowSnapshot {
    pub id: WindowId,
    pub manager: WindowManagerSnapshot,
    #[serde(default)]
    pub zoomed: bool,
    #[serde(default)]
    pub synchronize_panes: bool,
    #[serde(default)]
    pub zoom_snapshot: Option<WindowManagerSnapshot>,
}

pub(super) struct SessionWindow {
    pub(super) id: WindowId,
    pub(super) manager: WindowManager,
    pub(super) zoomed: bool,
    pub(super) synchronize_panes: bool,
    pub(super) zoom_snapshot: Option<WindowManagerSnapshot>,
}

pub struct SessionManager {
    pub(super) options: SessionOptions,
    pub(super) pane_factory: Arc<dyn PaneFactory>,
    pub(super) panes: HashMap<PaneId, Pane>,
    pub(super) windows: Vec<SessionWindow>,
    pub(super) active_window: usize,
    pub(super) next_pane_id: PaneId,
    pub(super) next_window_id: WindowId,
    pub(super) session_name: String,
    pub(super) pending_passthrough: Vec<Vec<u8>>,
    pub(super) pending_terminal_events: Vec<PaneTerminalEvent>,
}

impl SessionManager {
    pub fn new(options: SessionOptions, cols: u16, rows: u16) -> io::Result<Self> {
        Self::with_factory(options, Arc::new(PtyPaneFactory), cols, rows)
    }

    pub fn with_factory(
        options: SessionOptions,
        pane_factory: Arc<dyn PaneFactory>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        let area = workspace_area(cols, rows);
        let first_pane_id = 1;

        let first_pane = spawn_pane(
            &options,
            &*pane_factory,
            first_pane_id,
            area.width.max(1),
            area.height.max(1),
        )?;

        let mut panes = HashMap::new();
        panes.insert(first_pane_id, first_pane);

        let session_name = options.session_name.clone();

        Ok(Self {
            options,
            pane_factory,
            panes,
            windows: vec![SessionWindow {
                id: 1,
                manager: WindowManager::new(first_pane_id),
                zoomed: false,
                synchronize_panes: false,
                zoom_snapshot: None,
            }],
            active_window: 0,
            next_pane_id: first_pane_id + 1,
            next_window_id: 2,
            session_name,
            pending_passthrough: Vec::new(),
            pending_terminal_events: Vec::new(),
        })
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn rename_session(&mut self, name: String) {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            self.session_name = trimmed.to_string();
        }
    }

    pub fn set_suppress_prompt_eol_marker(&mut self, suppress: bool) {
        self.options.suppress_prompt_eol_marker = suppress;
    }

    pub fn suppress_prompt_eol_marker(&self) -> bool {
        self.options.suppress_prompt_eol_marker
    }

    pub fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.options.allow_passthrough = allow_passthrough;
        for pane in self.panes.values_mut() {
            pane.set_allow_passthrough(allow_passthrough);
        }
        if !allow_passthrough {
            self.pending_passthrough.clear();
        }
    }

    pub fn allow_passthrough(&self) -> bool {
        self.options.allow_passthrough
    }

    /// Update the host terminal default colors on every pane (and on the
    /// options template so panes spawned later inherit them).
    pub fn set_host_colors(&mut self, colors: HostColors) {
        self.options.host_colors = colors;
        for pane in self.panes.values_mut() {
            pane.set_host_colors(colors);
        }
    }

    /// Host terminal default colors currently applied to panes.
    pub fn host_colors(&self) -> HostColors {
        self.options.host_colors
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.active_window()
            .and_then(|window| window.manager.focused_pane_id())
    }

    pub fn focused_window_id(&self) -> Option<WindowId> {
        self.active_window().map(|window| window.id)
    }

    pub fn focused_pane_closed(&mut self) -> bool {
        let Some(pane_id) = self.focused_pane_id() else {
            return false;
        };
        self.panes
            .get_mut(&pane_id)
            .map(Pane::is_closed)
            .unwrap_or(false)
    }

    pub fn pane_exists(&self, pane_id: PaneId) -> bool {
        self.panes.contains_key(&pane_id)
    }

    pub fn pane_closed(&mut self, pane_id: PaneId) -> bool {
        self.panes.get_mut(&pane_id).is_some_and(Pane::is_closed)
    }

    pub fn focused_window_number(&self) -> Option<usize> {
        (!self.windows.is_empty()).then_some(self.active_window + 1)
    }

    pub fn poll_output(&mut self) -> bool {
        !self.poll_output_changed_panes().is_empty()
    }

    /// Poll all panes for pending output and return the ids of panes whose
    /// terminal content changed.
    pub fn poll_output_changed_panes(&mut self) -> Vec<PaneId> {
        let mut changed_panes = Vec::new();
        let mut pane_ids = self.panes.keys().copied().collect::<Vec<_>>();
        pane_ids.sort_unstable();

        for pane_id in pane_ids {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            if pane.poll_output() {
                changed_panes.push(pane_id);
            }
            self.pending_passthrough.extend(pane.take_passthrough());
            self.pending_terminal_events.extend(
                pane.take_terminal_events()
                    .into_iter()
                    .map(|event| PaneTerminalEvent { pane_id, event }),
            );
        }
        changed_panes
    }

    pub fn take_passthrough_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_passthrough)
    }

    pub fn take_terminal_events(&mut self) -> Vec<PaneTerminalEvent> {
        std::mem::take(&mut self.pending_terminal_events)
    }

    pub fn send_to_focused(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(pane_id) = self.focused_pane_id() else {
            return Ok(());
        };
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.write(bytes)?;
        }
        Ok(())
    }

    pub fn send_to_active_window(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let pane_ids = self.active_window_pane_ids();
        let mut sent = 0usize;
        for pane_id in pane_ids {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.write(bytes)?;
                sent += 1;
            }
        }
        Ok(sent)
    }

    pub fn send_to_pane(&mut self, pane_id: PaneId, bytes: &[u8]) -> io::Result<()> {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("pane {pane_id} not found"),
            ));
        };
        pane.write(bytes)
    }

    pub fn focused_scrollback_text(&self) -> Option<String> {
        let pane_id = self.focused_pane_id()?;
        self.panes.get(&pane_id).map(Pane::scrollback_text)
    }

    /// Kitty keyboard protocol flags enabled by the focused pane's guest
    /// (0 when the protocol is not in use or no pane has focus).
    pub fn focused_kitty_keyboard_flags(&self) -> u8 {
        self.focused_pane_id()
            .and_then(|pane_id| self.panes.get(&pane_id))
            .map(Pane::kitty_keyboard_flags)
            .unwrap_or(0)
    }

    /// Whether the focused pane's guest program enabled bracketed paste.
    pub fn focused_bracketed_paste(&self) -> bool {
        self.focused_pane_id()
            .and_then(|pane_id| self.panes.get(&pane_id))
            .is_some_and(Pane::bracketed_paste)
    }

    /// Whether any pane in the active window is holding frame output via
    /// synchronized output (DECSET 2026).
    pub fn active_window_sync_output_hold(&self) -> bool {
        self.active_window_pane_ids()
            .iter()
            .filter_map(|pane_id| self.panes.get(pane_id))
            .any(Pane::synchronized_output_active)
    }

    /// Earliest instant at which an active synchronized-output hold in the
    /// active window times out; `None` when no pane is holding. Lets the
    /// server loop wake exactly when a deferred render becomes flushable.
    pub fn active_window_sync_output_deadline(&self) -> Option<std::time::Instant> {
        self.active_window_pane_ids()
            .iter()
            .filter_map(|pane_id| self.panes.get(pane_id))
            .filter_map(Pane::sync_output_deadline)
            .min()
    }

    /// Whether the pane's guest program requested mouse reporting.
    pub fn pane_wants_mouse_reporting(&self, pane_id: PaneId) -> bool {
        self.panes
            .get(&pane_id)
            .is_some_and(Pane::wants_mouse_reporting)
    }

    /// Whether any pane in the active window requested mouse reporting
    /// (DECSET 9/1000/1002/1003). Drives host-terminal mouse capture: while
    /// no guest wants the mouse and spectra's own mouse handling is off,
    /// leaving the host uncaptured keeps native terminal features (e.g.
    /// ghostty link clicks) working.
    pub fn active_window_wants_mouse_reporting(&self) -> bool {
        self.active_window_pane_ids()
            .iter()
            .any(|pane_id| self.pane_wants_mouse_reporting(*pane_id))
    }

    /// Encode a mouse event for the pane's guest program, honouring its
    /// requested protocol/encoding. Returns `None` when the guest did not
    /// ask for this kind of event.
    pub fn pane_mouse_report(
        &self,
        pane_id: PaneId,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: usize,
        row: usize,
    ) -> Option<Vec<u8>> {
        self.panes
            .get(&pane_id)
            .and_then(|pane| pane.encode_mouse_event(kind, modifiers, col, row))
    }

    pub fn focused_history_lines(&self) -> Option<Vec<String>> {
        let pane_id = self.focused_pane_id()?;
        self.panes.get(&pane_id).map(Pane::history_lines)
    }

    pub fn focused_history_cells(&self) -> Option<Vec<Vec<StyledCell>>> {
        let pane_id = self.focused_pane_id()?;
        self.panes.get(&pane_id).map(Pane::history_cells)
    }

    pub fn focused_view_row_origin(&self, view_rows: usize) -> Option<usize> {
        let pane_id = self.focused_pane_id()?;
        self.panes
            .get(&pane_id)
            .map(|pane| pane.view_row_origin_for(view_rows))
    }

    pub fn pane_view_row_origin(&self, pane_id: PaneId, view_rows: usize) -> Option<usize> {
        self.panes
            .get(&pane_id)
            .map(|pane| pane.view_row_origin_for(view_rows))
    }

    pub fn focused_cursor_absolute_position(&self) -> Option<(usize, usize)> {
        let pane_id = self.focused_pane_id()?;
        self.panes.get(&pane_id).map(Pane::cursor_absolute_position)
    }

    pub fn pane_total_lines(&self, pane_id: PaneId) -> Option<usize> {
        self.panes.get(&pane_id).map(Pane::total_lines)
    }

    pub fn pane_screen_rows(&self, pane_id: PaneId) -> Option<usize> {
        self.panes.get(&pane_id).map(Pane::screen_rows)
    }

    /// The pane's visible screen rows as text, top to bottom (independent of
    /// any scrollback view offset).
    pub fn pane_screen_lines(&self, pane_id: PaneId) -> Option<Vec<String>> {
        let pane = self.panes.get(&pane_id)?;
        Some(
            (0..pane.screen_rows())
                .map(|row| pane.row_text(row))
                .collect(),
        )
    }

    /// Pid of the process spawned for the pane, when the backend knows it.
    pub fn pane_child_pid(&self, pane_id: PaneId) -> Option<u32> {
        self.panes.get(&pane_id)?.child_pid()
    }

    /// Everything the live server handoff needs to transfer one pane:
    /// its PTY master fd, child pid, and the raw replay tail. Errors when
    /// the pane has no transferable fd (fake/test backends).
    #[cfg(unix)]
    pub fn pane_handoff_export(&self, pane_id: PaneId) -> io::Result<PaneHandoffExport> {
        let pane = self.panes.get(&pane_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("pane {pane_id} not found"))
        })?;
        let master_fd = pane.handoff_master_fd().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("pane {pane_id} has no transferable PTY fd"),
            )
        })?;
        Ok(PaneHandoffExport {
            master_fd,
            child_pid: pane.child_pid(),
            replay: pane.replay_tail().to_vec(),
        })
    }

    /// Disarm kill-on-drop for every pane child; the outgoing server calls
    /// this only after its successor acked receipt of all PTY fds.
    pub fn disarm_pane_children(&mut self) {
        for pane in self.panes.values_mut() {
            pane.disarm_child_kill();
        }
    }

    /// Feed transferred replay bytes into one pane's terminal state after a
    /// handoff. Returns false when the pane does not exist.
    pub fn feed_pane_replay(&mut self, pane_id: PaneId, bytes: &[u8]) -> bool {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return false;
        };
        pane.feed_replay(bytes);
        true
    }

    /// Replace the factory used for future pane spawns. The successor server
    /// restores sessions through a fd-adopting factory and then swaps back to
    /// the real PTY factory so later splits spawn fresh shells.
    pub fn set_pane_factory(&mut self, pane_factory: Arc<dyn PaneFactory>) {
        self.pane_factory = pane_factory;
    }

    pub fn pane_absolute_row_cells(
        &self,
        pane_id: PaneId,
        absolute_row: usize,
    ) -> Option<Vec<StyledCell>> {
        self.panes
            .get(&pane_id)
            .map(|pane| pane.absolute_row_cells(absolute_row))
    }

    pub fn scroll_focused_pane(&mut self, lines: isize, view_rows: usize) {
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.scroll_view(lines, view_rows);
        }
    }

    pub fn reset_focused_pane_view_scroll(&mut self) -> bool {
        let Some(pane_id) = self.focused_pane_id() else {
            return false;
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return false;
        };
        pane.reset_view_scroll()
    }

    pub fn pane_history_tail_lines(
        &self,
        pane_id: PaneId,
        max_lines: usize,
    ) -> Option<Vec<String>> {
        self.panes
            .get(&pane_id)
            .map(|pane| pane.history_tail_lines(max_lines))
    }

    pub fn focused_export_text_hard_lf(&self) -> Option<String> {
        let pane_id = self.focused_pane_id()?;
        self.panes.get(&pane_id).map(Pane::export_text_hard_lf)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.apply_layout_sizes(cols, rows)
    }

    pub(super) fn apply_layout_sizes(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let area = workspace_area(cols, rows);
        let layouts: Vec<Layout> = self
            .windows
            .iter()
            .map(|window| window.manager.layout(area))
            .collect();
        for layout in layouts {
            for PaneLayout { pane_id, rect, .. } in layout.panes {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.resize(rect.width.max(1), rect.height.max(1))?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn active_window_pane_ids(&self) -> Vec<PaneId> {
        let Some(window) = self.active_window() else {
            return Vec::new();
        };

        if window.zoomed
            && let Some(snapshot) = window.zoom_snapshot.as_ref()
        {
            return snapshot.ordered_pane_ids();
        }

        window.manager.ordered_pane_ids()
    }

    pub(super) fn ensure_active_window_unzoomed(&mut self) -> Result<(), String> {
        let window = self.active_window_mut()?;
        Self::restore_zoom(window)
    }

    pub(super) fn restore_zoom(window: &mut SessionWindow) -> Result<(), String> {
        if !window.zoomed {
            return Ok(());
        }
        let snapshot = window
            .zoom_snapshot
            .take()
            .ok_or_else(|| "zoom snapshot missing".to_string())?;
        window.manager = WindowManager::from_snapshot(snapshot)?;
        window.zoomed = false;
        Ok(())
    }

    pub(super) fn active_window(&self) -> Option<&SessionWindow> {
        self.windows.get(self.active_window)
    }

    pub(super) fn active_window_mut(&mut self) -> Result<&mut SessionWindow, String> {
        self.windows
            .get_mut(self.active_window)
            .ok_or_else(|| "No windows available".to_string())
    }
}

pub(super) fn spawn_pane(
    options: &SessionOptions,
    pane_factory: &dyn PaneFactory,
    pane_id: PaneId,
    width: usize,
    height: usize,
) -> io::Result<Pane> {
    let backend = pane_factory.spawn(&PaneSpawnConfig {
        shell: options.shell.clone(),
        cwd: options.cwd.clone(),
        command: options.command.clone(),
        suppress_prompt_eol_marker: options.suppress_prompt_eol_marker,
        cols: width as u16,
        rows: height as u16,
        pane_id,
        session_id: options.session_id.clone(),
    })?;
    let mut pane = Pane::new(width, height, options.allow_passthrough, backend);
    pane.set_host_colors(options.host_colors);
    Ok(pane)
}

pub(super) fn workspace_area(cols: u16, rows: u16) -> PaneRect {
    let width = usize::from(cols).max(1);
    let full_height = usize::from(rows).max(1);
    let height = full_height.saturating_sub(1).max(1);
    PaneRect {
        x: 0,
        y: 0,
        width,
        height,
    }
}
