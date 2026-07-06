use super::*;

impl App {
    pub fn take_render_snapshot(&mut self) -> Option<RenderSnapshot> {
        let snapshot = self.render_snapshot_for_client(LOCAL_CLIENT_ID)?;
        self.finish_render_cycle();
        Some(snapshot)
    }

    pub fn has_pending_render(&self) -> bool {
        self.needs_render
    }

    /// Whether rendering should be deferred because a pane in the active
    /// window requested synchronized output (DECSET 2026). The hold is
    /// bounded by [`crate::session::terminal_state::SYNC_OUTPUT_TIMEOUT`].
    pub fn render_hold_for_sync_output(&self) -> bool {
        self.current_session().active_window_sync_output_hold()
    }

    pub fn render_snapshot_for_client(&mut self, client_id: ClientId) -> Option<RenderSnapshot> {
        if !self.needs_render {
            return None;
        }

        let snapshot = self.with_client_context(client_id, |app| {
            // The pane this client is looking at counts as seen: its agent
            // result must never render as "done".
            app.mark_focused_agent_seen();
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
                wants_mouse_capture: app.wants_host_mouse_capture(),
            }
        });
        Some(snapshot)
    }

    /// Whether the host terminal of the active client should capture mouse
    /// events: spectra's own mouse handling is enabled, or a guest program
    /// in the active window requested mouse reporting. While false, the
    /// host terminal keeps native mouse behaviour (selection, link clicks).
    pub(super) fn wants_host_mouse_capture(&self) -> bool {
        self.mouse_enabled || self.current_session().active_window_wants_mouse_reporting()
    }

    pub fn finish_render_cycle(&mut self) {
        self.needs_render = false;
        self.needs_full_clear = false;
    }

    pub fn request_render(&mut self, full_clear: bool) {
        self.needs_render = true;
        if full_clear {
            self.needs_full_clear = true;
        }
    }

    pub(super) fn prune_side_window_tree_state(&mut self) {
        if self.sessions.is_empty() {
            self.view.side_window_tree_open = false;
        }
    }

    pub(super) fn side_window_tree_is_open(&self) -> bool {
        self.view.side_window_tree_open
    }

    /// Every side-tree row in display order: a header per session followed by
    /// that session's windows. Shared by rendering and click hit-testing.
    pub(super) fn side_window_tree_layout_rows(&self) -> Vec<SideTreeLayoutRow> {
        let mut rows = Vec::new();
        for (session_index, managed) in self.sessions.iter().enumerate() {
            rows.push(SideTreeLayoutRow {
                session_index,
                window: None,
            });
            for entry in managed.session.window_entries() {
                rows.push(SideTreeLayoutRow {
                    session_index,
                    window: Some(SideTreeWindowRow {
                        window_number: entry.index,
                        window_id: entry.window_id,
                        pane_ids: entry.pane_ids.clone(),
                        focused: entry.focused,
                    }),
                });
            }
        }
        rows
    }

    /// Row index of the window focused in the active session; falls back to the
    /// first window row (never a header) when nothing matches.
    fn side_window_tree_selected_row(&self, rows: &[SideTreeLayoutRow]) -> usize {
        rows.iter()
            .position(|row| {
                row.session_index == self.view.active_session
                    && row.window.as_ref().is_some_and(|window| window.focused)
            })
            .or_else(|| rows.iter().position(|row| row.window.is_some()))
            .unwrap_or(0)
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

    pub(super) fn effective_pane_cols_for_view(cols: u16, side_window_tree_open: bool) -> u16 {
        // The reserve equals the sidebar rect's pane offset, so pane sizing
        // and pane shifting cannot drift apart.
        let reserved = if side_window_tree_open {
            Self::side_window_tree_width_for_cols(cols)
                .map(|width| {
                    crate::ui::render::SidebarRect::left_edge(usize::from(width)).pane_x_offset()
                })
                .unwrap_or(0)
        } else {
            0
        };
        u16::try_from(usize::from(cols).saturating_sub(reserved)).unwrap_or(0)
    }

    pub(super) fn current_effective_pane_dims(&self) -> (u16, u16) {
        (
            Self::effective_pane_cols_for_view(self.view.cols, self.view.side_window_tree_open),
            self.view.rows,
        )
    }

    fn side_window_tree_width(&self) -> Option<usize> {
        Self::side_window_tree_width_for_cols(self.view.cols).map(usize::from)
    }

    pub(super) fn side_window_tree_overlay(&self) -> Option<crate::ui::render::SideWindowTree> {
        if !self.side_window_tree_is_open() {
            return None;
        }
        let width = self.side_window_tree_width()?;
        let rows = self.side_window_tree_layout_rows();
        if rows.iter().all(|row| row.window.is_none()) {
            return None;
        }
        let selected = self.side_window_tree_selected_row(&rows);
        let entries = rows
            .iter()
            .map(|row| match &row.window {
                None => crate::ui::render::SideTreeEntry {
                    label: self.sessions[row.session_index]
                        .session
                        .session_name()
                        .to_string(),
                    indicator: None,
                    is_header: true,
                },
                Some(window) => {
                    let custom_name = self
                        .effective_window_name(row.session_index, window.window_id)
                        .filter(|name| !name.is_empty());
                    let label = if let Some(name) = custom_name {
                        format!("w{}:{name}", window.window_number)
                    } else {
                        format!("w{}", window.window_number)
                    };
                    crate::ui::render::SideTreeEntry {
                        label,
                        indicator: Self::window_agent_indicator(
                            &self.sessions[row.session_index],
                            &window.pane_ids,
                        ),
                        is_header: false,
                    }
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

    /// Aggregate a window's per-pane agent display states into one sidebar
    /// marker, worst state first (Blocked > Working > Done > Idle). Unknown
    /// presence and agent-free windows carry no marker.
    pub(super) fn window_agent_indicator(
        managed: &ManagedSession,
        pane_ids: &[usize],
    ) -> Option<crate::ui::render::AgentIndicator> {
        pane_ids
            .iter()
            .filter_map(|pane_id| managed.agents.display_state(*pane_id))
            .max()
            .and_then(crate::ui::render::AgentIndicator::for_state)
    }

    /// Resolve a sidebar click to the `(session_index, window_number)` it lands
    /// on, or `None` for the divider, a session header, or empty space.
    pub(super) fn side_window_tree_target_at(
        &self,
        side: &crate::ui::render::SideWindowTree,
        col: u16,
        row: u16,
    ) -> Option<(usize, usize)> {
        let col = usize::from(col);
        let row = usize::from(row);
        let workspace_rows = usize::from(self.view.rows.saturating_sub(1));

        // Clicks must land on entry text area, not the divider/header/status rows.
        if workspace_rows <= 1
            || !side.rect().contains_content_col(col)
            || row == 0
            || row >= workspace_rows
        {
            return None;
        }

        let rows = self.side_window_tree_layout_rows();
        if rows.is_empty() {
            return None;
        }

        let content_height = workspace_rows.saturating_sub(1);
        if content_height == 0 {
            return None;
        }

        let selected = self.side_window_tree_selected_row(&rows);
        let start = Self::side_window_tree_scroll_start(selected, rows.len(), content_height);
        let entry_index = start + row.saturating_sub(1);

        rows.get(entry_index).and_then(|entry| {
            entry
                .window
                .as_ref()
                .map(|window| (entry.session_index, window.window_number))
        })
    }

    fn shift_frame_for_side_window_tree(
        &self,
        frame: &mut crate::session::manager::RenderFrame,
        rect: crate::ui::render::SidebarRect,
    ) {
        let offset = rect.pane_x_offset();
        if offset == 0 {
            return;
        }
        for pane in &mut frame.panes {
            pane.rect.x = pane.rect.x.saturating_add(offset);
        }
        for divider in &mut frame.dividers {
            divider.x = divider.x.saturating_add(offset);
        }
        if let Some((x, y)) = frame.focused_cursor {
            frame.focused_cursor = Some((x.saturating_add(offset as u16), y));
        }
    }

    pub(super) fn pane_frame_for_current_view_with_sidebar(
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
            self.shift_frame_for_side_window_tree(&mut frame, tree.rect());
        }
        frame
    }

    pub(super) fn toggle_side_window_tree(&mut self) {
        self.prune_side_window_tree_state();
        self.view.side_window_tree_open = !self.view.side_window_tree_open;
        if let Err(err) = self.resize_sessions_to_max_client_viewport() {
            self.set_message(&format!("resize failed: {err}"), Duration::from_secs(3));
            self.write_log(&format!("side window tree resize failed: {err}"));
        }
        self.needs_full_clear = true;
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

    fn keybindings_overlay(&self, state: &KeybindingsState) -> crate::ui::render::SystemOverlay {
        let candidates = Self::keybinding_candidates(state);
        let selected = state.selected.min(candidates.len().saturating_sub(1));
        let key_width = candidates
            .iter()
            .filter_map(|&index| state.rows.get(index))
            .map(|row| row.keys.chars().count())
            .max()
            .unwrap_or(0)
            .min(18);
        let candidate_labels = candidates
            .iter()
            .filter_map(|&index| state.rows.get(index))
            .map(|row| format!("{:<key_width$}  {}", row.keys, row.description))
            .collect::<Vec<_>>();
        let preview_lines = candidates
            .get(selected)
            .and_then(|&index| state.rows.get(index))
            .map(|row| {
                vec![
                    format!("key : {}", row.keys),
                    format!("does: {}", row.description),
                    String::new(),
                    format!("prefix key: {}", self.view.keys.prefix_key_display()),
                    "j/k or arrows move, / filter, Esc/q close".to_string(),
                ]
            })
            .unwrap_or_else(|| vec!["no shortcuts matched".to_string()]);

        crate::ui::render::SystemOverlay {
            title: "keys".to_string(),
            query: state.query_input.text.clone(),
            query_cursor_pos: state.query_input.cursor,
            query_active: state.query_active,
            candidates: candidate_labels,
            selected,
            selected_cursor_pos: None,
            preview_lines,
            preview_from_tail: false,
        }
    }

    pub(super) fn system_overlay(&self) -> Option<crate::ui::render::SystemOverlay> {
        match &self.view.input_mode {
            InputMode::RenameTreeItem {
                target,
                buffer,
                return_tree: Some(state),
            } => self.system_tree_overlay_for_state(state, Some((*target, buffer))),
            InputMode::SystemTree { state } => self.system_tree_overlay_for_state(state, None),
            InputMode::CommandPalette { state } => {
                let entries = Self::command_palette_entries_for(self.command_palette_context());
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
            InputMode::Keybindings { state } => Some(self.keybindings_overlay(state)),
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

    pub(super) fn status_line(&self) -> String {
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
            InputMode::Keybindings { state } => {
                let mode = if state.query_active {
                    "filter"
                } else {
                    "browse"
                };
                return format!(
                    "keybindings ({mode}): j/k or Up/Down move, / filter, Ctrl+n/p nav, Enter/Esc leave filter, Esc/q close"
                );
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
            ("{agent}", self.focused_agent_token()),
            ("{update}", self.update_token()),
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
}
