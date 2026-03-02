use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

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
    pub suppress_prompt_eol_marker: bool,
    pub allow_passthrough: bool,
}

impl SessionOptions {
    pub fn from_cli(shell: Option<String>, cwd: Option<PathBuf>, command: Vec<String>) -> Self {
        Self {
            shell: shell.unwrap_or_else(default_shell),
            cwd,
            command,
            session_name: "main".to_string(),
            suppress_prompt_eol_marker: false,
            allow_passthrough: true,
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
        let mut changed = false;
        let mut pane_ids = self.panes.keys().copied().collect::<Vec<_>>();
        pane_ids.sort_unstable();

        for pane_id in pane_ids {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            changed |= pane.poll_output();
            self.pending_passthrough.extend(pane.take_passthrough());
            self.pending_terminal_events.extend(
                pane.take_terminal_events()
                    .into_iter()
                    .map(|event| PaneTerminalEvent { pane_id, event }),
            );
        }
        changed
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
    })?;
    Ok(Pane::new(width, height, options.allow_passthrough, backend))
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
