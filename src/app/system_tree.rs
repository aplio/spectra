use std::collections::HashSet;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::filtering::fzf_style_match;

use super::types::*;
use super::{App, session_id_for};

impl App {
    pub(super) fn handle_system_tree_mode_key(
        &mut self,
        mode: InputMode,
        key: KeyEvent,
    ) -> InputMode {
        match mode {
            InputMode::RenameTreeItem {
                target,
                mut buffer,
                return_tree,
            } => match key.code {
                KeyCode::Esc => {
                    self.set_message("rename cancelled", Duration::from_secs(2));
                    self.return_after_rename(return_tree)
                }
                KeyCode::Enter => {
                    let next_name = buffer.trim().to_string();
                    self.apply_rename_target(target, &next_name);
                    self.return_after_rename(return_tree)
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    InputMode::RenameTreeItem {
                        target,
                        buffer,
                        return_tree,
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    buffer.push(ch);
                    InputMode::RenameTreeItem {
                        target,
                        buffer,
                        return_tree,
                    }
                }
                _ => InputMode::RenameTreeItem {
                    target,
                    buffer,
                    return_tree,
                },
            },
            InputMode::ConfirmDelete {
                target,
                label,
                return_tree,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    match self.execute_tree_delete(target) {
                        Ok(msg) => self.set_message(&msg, Duration::from_secs(2)),
                        Err(msg) => self.set_message(&msg, Duration::from_secs(3)),
                    }
                    let state = self.normalize_system_tree_state(return_tree);
                    InputMode::SystemTree { state }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.set_message("delete cancelled", Duration::from_secs(2));
                    InputMode::SystemTree { state: return_tree }
                }
                _ => InputMode::ConfirmDelete {
                    target,
                    label,
                    return_tree,
                },
            },
            InputMode::SystemTree { mut state } => {
                state = self.normalize_system_tree_state(state);
                let rows = self.system_tree_rows(&state);
                if rows.is_empty() {
                    InputMode::Normal
                } else {
                    let candidates = self.system_tree_candidates(&state, &rows);
                    if let Some((_, selected)) = Self::selected_tree_candidate(&state, &candidates)
                    {
                        state.cursor_row = selected.row_index;
                    }

                    let mut control_handled = false;
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            KeyCode::Left if state.query_active => {
                                state.query_input.move_word_left();
                                control_handled = true;
                            }
                            KeyCode::Right if state.query_active => {
                                state.query_input.move_word_right();
                                control_handled = true;
                            }
                            KeyCode::Char(ch) => match ch.to_ascii_lowercase() {
                                'n' | 'j' => {
                                    if !candidates.is_empty() {
                                        if state.query_active {
                                            state.query_active = false;
                                        }
                                        Self::tree_move_down(&mut state, &candidates);
                                    }
                                    control_handled = true;
                                }
                                'p' => {
                                    if !candidates.is_empty() {
                                        if state.query_active {
                                            state.query_active = false;
                                        }
                                        Self::tree_move_up(&mut state, &candidates);
                                    }
                                    control_handled = true;
                                }
                                'f' if state.query_active => {
                                    state.query_input.move_right();
                                    control_handled = true;
                                }
                                'b' if state.query_active => {
                                    state.query_input.move_left();
                                    control_handled = true;
                                }
                                'a' if state.query_active => {
                                    state.query_input.move_start();
                                    control_handled = true;
                                }
                                'e' if state.query_active => {
                                    state.query_input.move_end();
                                    control_handled = true;
                                }
                                'w' if state.query_active => {
                                    if state.query_input.delete_prev_word() {
                                        state = self.normalize_system_tree_state(state);
                                    }
                                    control_handled = true;
                                }
                                'k' if state.query_active => {
                                    if state.query_input.delete_to_end() {
                                        state = self.normalize_system_tree_state(state);
                                    }
                                    control_handled = true;
                                }
                                'u' if state.query_active => {
                                    state.query_input.clear();
                                    state = self.normalize_system_tree_state(state);
                                    control_handled = true;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }

                    if control_handled {
                        InputMode::SystemTree { state }
                    } else {
                        match key.code {
                            KeyCode::Esc => {
                                if state.query_active {
                                    state.query_active = false;
                                    InputMode::SystemTree { state }
                                } else {
                                    InputMode::Normal
                                }
                            }
                            KeyCode::Char('/')
                                if !state.query_active
                                    && !key
                                        .modifiers
                                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                state.query_active = true;
                                state.query_input.move_end();
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Backspace if state.query_active => {
                                if state.query_input.backspace() {
                                    state = self.normalize_system_tree_state(state);
                                }
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Char(ch)
                                if state.query_active
                                    && !key
                                        .modifiers
                                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                state.query_input.insert_char(ch);
                                state = self.normalize_system_tree_state(state);
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Enter => {
                                if let Some((_, selected)) =
                                    Self::selected_tree_candidate(&state, &candidates)
                                {
                                    self.apply_tree_selection(&rows[selected.row_index]);
                                    InputMode::Normal
                                } else {
                                    InputMode::SystemTree { state }
                                }
                            }
                            KeyCode::Char(ch)
                                if !state.query_active
                                    && ch.eq_ignore_ascii_case(&'r')
                                    && !key
                                        .modifiers
                                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                if let Some((_, selected)) =
                                    Self::selected_tree_candidate(&state, &candidates)
                                {
                                    let target =
                                        Self::rename_target_for_row(&rows[selected.row_index]);
                                    let buffer = self.rename_buffer_for_target(target);
                                    InputMode::RenameTreeItem {
                                        target,
                                        buffer,
                                        return_tree: Some(state),
                                    }
                                } else {
                                    InputMode::SystemTree { state }
                                }
                            }
                            KeyCode::Up if state.query_active => InputMode::SystemTree { state },
                            KeyCode::Up => {
                                if let Some((selected, _)) =
                                    Self::selected_tree_candidate(&state, &candidates)
                                {
                                    if selected == 0 {
                                        state.query_active = true;
                                    } else {
                                        Self::tree_move_up(&mut state, &candidates);
                                    }
                                }
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Down if state.query_active => {
                                if let Some(first) = candidates.first() {
                                    state.cursor_row = first.row_index;
                                    state.query_active = false;
                                }
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Down => {
                                Self::tree_move_down(&mut state, &candidates);
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Left if state.query_active => {
                                state.query_input.move_left();
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Left | KeyCode::BackTab if !state.query_active => {
                                if !candidates.is_empty() {
                                    Self::tree_move_left(&mut state, &rows);
                                    state = self.normalize_system_tree_state(state);
                                }
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Right if state.query_active => {
                                state.query_input.move_right();
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Right | KeyCode::Tab if !state.query_active => {
                                if !candidates.is_empty() {
                                    Self::tree_move_right(&mut state, &rows);
                                    state = self.normalize_system_tree_state(state);
                                }
                                InputMode::SystemTree { state }
                            }
                            KeyCode::Backspace if !state.query_active => {
                                if let Some((_, selected)) =
                                    Self::selected_tree_candidate(&state, &candidates)
                                {
                                    let row = &rows[selected.row_index];
                                    let label = row.label.trim().to_string();
                                    InputMode::ConfirmDelete {
                                        target: row.kind,
                                        label,
                                        return_tree: state,
                                    }
                                } else {
                                    InputMode::SystemTree { state }
                                }
                            }
                            _ => InputMode::SystemTree { state },
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    pub(super) fn open_system_tree(&mut self) {
        let state = self.normalize_system_tree_state(self.default_system_tree_state());
        self.view.input_mode = InputMode::SystemTree { state };
    }

    pub(super) fn return_after_rename(
        &mut self,
        return_tree: Option<SystemTreeState>,
    ) -> InputMode {
        if let Some(state) = return_tree {
            InputMode::SystemTree {
                state: self.normalize_system_tree_state(state),
            }
        } else {
            InputMode::Normal
        }
    }

    pub(super) fn rename_target_for_row(row: &TreeRow) -> RenameTarget {
        match row.kind {
            TreeRowKind::Session { session_index } => RenameTarget::Session { session_index },
            TreeRowKind::Window {
                session_index,
                window_id,
                ..
            } => RenameTarget::Window {
                session_index,
                window_id,
            },
            TreeRowKind::Pane {
                session_index,
                pane_id,
            } => RenameTarget::Pane {
                session_index,
                pane_id,
            },
        }
    }

    pub(super) fn rename_buffer_for_target(&self, target: RenameTarget) -> String {
        match target {
            RenameTarget::Session { session_index } => self
                .sessions
                .get(session_index)
                .map(|managed| managed.session.session_name().to_string())
                .unwrap_or_default(),
            RenameTarget::Window {
                session_index,
                window_id,
            } => self
                .sessions
                .get(session_index)
                .and_then(|managed| managed.window_names.get(&window_id).cloned())
                .unwrap_or_default(),
            RenameTarget::Pane {
                session_index,
                pane_id,
            } => self
                .sessions
                .get(session_index)
                .and_then(|managed| managed.pane_names.get(&pane_id).cloned())
                .unwrap_or_default(),
        }
    }

    pub(super) fn apply_rename_target(&mut self, target: RenameTarget, next_name: &str) {
        match target {
            RenameTarget::Session { session_index } => {
                if session_index >= self.sessions.len() {
                    self.set_message("rename target missing", Duration::from_secs(2));
                    return;
                }
                if next_name.trim().is_empty() {
                    self.set_message("rename cancelled", Duration::from_secs(2));
                    return;
                }
                let ordinal = self.sessions[session_index].ordinal;
                let is_active = session_index == self.view.active_session;
                {
                    let managed = &mut self.sessions[session_index];
                    managed.session.rename_session(next_name.to_string());
                    managed.session_id = session_id_for(next_name, ordinal);
                }
                if is_active {
                    self.persist_active_session_info();
                    self.write_log(&format!("session renamed to {next_name}"));
                }
                self.set_message("session renamed", Duration::from_secs(3));
                self.persist_runtime_state();
            }
            RenameTarget::Window {
                session_index,
                window_id,
            } => {
                let Some(managed) = self.sessions.get_mut(session_index) else {
                    self.set_message("rename target missing", Duration::from_secs(2));
                    return;
                };
                let window_exists = managed
                    .session
                    .window_entries()
                    .iter()
                    .any(|entry| entry.window_id == window_id);
                if !window_exists {
                    self.set_message("rename target missing", Duration::from_secs(2));
                    return;
                }
                if next_name.trim().is_empty() {
                    managed.window_names.remove(&window_id);
                    self.set_message("window rename cleared", Duration::from_secs(3));
                } else {
                    managed
                        .window_names
                        .insert(window_id, next_name.to_string());
                    self.set_message("window renamed", Duration::from_secs(3));
                }
                self.persist_runtime_state();
            }
            RenameTarget::Pane {
                session_index,
                pane_id,
            } => {
                let Some(managed) = self.sessions.get_mut(session_index) else {
                    self.set_message("rename target missing", Duration::from_secs(2));
                    return;
                };
                let pane_exists = managed
                    .session
                    .window_entries()
                    .iter()
                    .any(|entry| entry.pane_ids.contains(&pane_id));
                if !pane_exists {
                    self.set_message("rename target missing", Duration::from_secs(2));
                    return;
                }
                if next_name.trim().is_empty() {
                    managed.pane_names.remove(&pane_id);
                    self.set_message("pane rename cleared", Duration::from_secs(3));
                } else {
                    managed.pane_names.insert(pane_id, next_name.to_string());
                    self.set_message("pane renamed", Duration::from_secs(3));
                }
                self.persist_runtime_state();
            }
        }
    }

    pub(super) fn sync_tree_names(&mut self) {
        for managed in &mut self.sessions {
            let entries = managed.session.window_entries();
            let valid_windows = entries
                .iter()
                .map(|entry| entry.window_id)
                .collect::<HashSet<_>>();
            let valid_panes = entries
                .iter()
                .flat_map(|entry| entry.pane_ids.iter().copied())
                .collect::<HashSet<_>>();
            managed
                .window_names
                .retain(|window_id, _| valid_windows.contains(window_id));
            managed
                .pane_names
                .retain(|pane_id, _| valid_panes.contains(pane_id));
            managed
                .window_auto_names
                .retain(|window_id, _| valid_windows.contains(window_id));
            managed
                .pane_auto_names
                .retain(|pane_id, _| valid_panes.contains(pane_id));
            managed
                .terminal_titles
                .retain(|pane_id, _| valid_panes.contains(pane_id));
            managed
                .cwd_fallbacks
                .retain(|pane_id, _| valid_panes.contains(pane_id));
        }
        self.prune_side_window_tree_state();
    }

    pub(super) fn default_system_tree_state(&self) -> SystemTreeState {
        let mut state = SystemTreeState::default();

        for (session_index, _) in self.sessions.iter().enumerate() {
            state.expanded_sessions.insert(session_index);
        }

        let rows = self.system_tree_rows(&state);
        state.cursor_row = self.default_tree_cursor(&rows).unwrap_or(0);
        state
    }

    pub(super) fn default_tree_cursor(&self, rows: &[TreeRow]) -> Option<usize> {
        let active_session = self.view.active_session;
        if let Some(focused_pane) = self.current_session().focused_pane_id()
            && let Some(row_index) = rows.iter().position(|row| {
                matches!(
                    row.kind,
                    TreeRowKind::Pane {
                        session_index,
                        pane_id,
                    } if session_index == active_session && pane_id == focused_pane
                )
            })
        {
            return Some(row_index);
        }

        if let Some(focused_window) = self.current_session().focused_window_number()
            && let Some(row_index) = rows.iter().position(|row| {
                matches!(
                    row.kind,
                    TreeRowKind::Window {
                        session_index,
                        window_number,
                        ..
                    } if session_index == active_session && window_number == focused_window
                )
            })
        {
            return Some(row_index);
        }

        rows.iter().position(|row| {
            matches!(
                row.kind,
                TreeRowKind::Session { session_index } if session_index == active_session
            )
        })
    }

    pub(super) fn normalize_system_tree_state(
        &mut self,
        mut state: SystemTreeState,
    ) -> SystemTreeState {
        self.sync_tree_names();
        state
            .expanded_sessions
            .retain(|session_index| *session_index < self.sessions.len());
        state.expanded_windows.retain(|key| {
            self.sessions
                .get(key.session_index)
                .is_some_and(|managed| key.window_index < managed.session.window_count())
        });

        let rows = self.system_tree_rows(&state);
        if rows.is_empty() {
            state.cursor_row = 0;
        } else {
            state.cursor_row = state.cursor_row.min(rows.len().saturating_sub(1));
            let candidates = self.system_tree_candidates(&state, &rows);
            if !candidates.is_empty()
                && !candidates
                    .iter()
                    .any(|candidate| candidate.row_index == state.cursor_row)
            {
                state.cursor_row = candidates[0].row_index;
            }
        }
        state
    }

    pub(super) fn system_tree_rows(&self, state: &SystemTreeState) -> Vec<TreeRow> {
        let mut rows = Vec::new();

        for (session_index, managed) in self.sessions.iter().enumerate() {
            let windows = managed.session.window_entries();
            let session_has_children = !windows.is_empty();
            let session_expanded =
                session_has_children && state.expanded_sessions.contains(&session_index);
            let session_label = format!(
                "{} session s{}:{}{}",
                tree_marker(session_has_children, session_expanded),
                session_index + 1,
                managed.session.session_name(),
                if session_index == self.view.active_session {
                    " *"
                } else {
                    ""
                }
            );
            rows.push(TreeRow {
                kind: TreeRowKind::Session { session_index },
                parent_row: None,
                has_children: session_has_children,
                expanded: session_expanded,
                label: session_label,
            });
            let session_row = rows.len().saturating_sub(1);

            if !session_expanded {
                continue;
            }

            let focused_pane = managed.session.focused_pane_id();
            for (window_index, window) in windows.into_iter().enumerate() {
                let key = TreeWindowKey {
                    session_index,
                    window_index,
                };
                let window_expanded = state.expanded_windows.contains(&key);
                let window_display = named_tree_id(
                    "w",
                    window.index,
                    self.effective_window_name(session_index, window.window_id),
                );
                let window_label = format!(
                    "  {} window {}{}",
                    tree_marker(!window.pane_ids.is_empty(), window_expanded),
                    window_display,
                    if session_index == self.view.active_session && window.focused {
                        " *"
                    } else {
                        ""
                    }
                );
                rows.push(TreeRow {
                    kind: TreeRowKind::Window {
                        session_index,
                        window_index,
                        window_number: window.index,
                        window_id: window.window_id,
                    },
                    parent_row: Some(session_row),
                    has_children: !window.pane_ids.is_empty(),
                    expanded: window_expanded,
                    label: window_label,
                });
                let window_row = rows.len().saturating_sub(1);

                if window_expanded {
                    for pane_id in window.pane_ids {
                        let pane_display = named_tree_id(
                            "p",
                            pane_id,
                            self.effective_pane_name(session_index, pane_id),
                        );
                        let pane_label = format!(
                            "      pane {}{}",
                            pane_display,
                            if focused_pane == Some(pane_id) {
                                " *"
                            } else {
                                ""
                            }
                        );
                        rows.push(TreeRow {
                            kind: TreeRowKind::Pane {
                                session_index,
                                pane_id,
                            },
                            parent_row: Some(window_row),
                            has_children: false,
                            expanded: false,
                            label: pane_label,
                        });
                    }
                }
            }
        }

        rows
    }

    pub(super) fn system_tree_candidates(
        &self,
        state: &SystemTreeState,
        rows: &[TreeRow],
    ) -> Vec<ScoredTreeCandidate> {
        let query = state.query_input.text.trim();
        if query.is_empty() {
            return rows
                .iter()
                .enumerate()
                .map(|(row_index, _)| ScoredTreeCandidate {
                    row_index,
                    score: 0,
                })
                .collect();
        }

        let mut scored = rows
            .iter()
            .enumerate()
            .filter_map(|(row_index, row)| {
                let key = self.tree_row_search_key(row);
                fzf_style_match(&key, query)
                    .map(|(score, _)| ScoredTreeCandidate { row_index, score })
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.row_index.cmp(&b.row_index))
        });
        scored
    }

    pub(super) fn selected_tree_candidate<'a>(
        state: &SystemTreeState,
        candidates: &'a [ScoredTreeCandidate],
    ) -> Option<(usize, &'a ScoredTreeCandidate)> {
        if candidates.is_empty() {
            return None;
        }
        candidates
            .iter()
            .position(|candidate| candidate.row_index == state.cursor_row)
            .map(|index| (index, &candidates[index]))
            .or(Some((0, &candidates[0])))
    }

    pub(super) fn tree_move_up(state: &mut SystemTreeState, candidates: &[ScoredTreeCandidate]) {
        let Some((selected, _)) = Self::selected_tree_candidate(state, candidates) else {
            return;
        };
        if selected > 0 {
            state.cursor_row = candidates[selected - 1].row_index;
        }
    }

    pub(super) fn tree_move_down(state: &mut SystemTreeState, candidates: &[ScoredTreeCandidate]) {
        let Some((selected, _)) = Self::selected_tree_candidate(state, candidates) else {
            return;
        };
        if selected + 1 < candidates.len() {
            state.cursor_row = candidates[selected + 1].row_index;
        }
    }

    pub(super) fn tree_row_id_and_name(&self, row: &TreeRow) -> (String, String) {
        match row.kind {
            TreeRowKind::Session { session_index } => {
                let id = format!("s{}", session_index + 1);
                let name = self
                    .sessions
                    .get(session_index)
                    .map(|managed| managed.session.session_name().to_string())
                    .unwrap_or_default();
                (id, name)
            }
            TreeRowKind::Window {
                session_index,
                window_number,
                window_id,
                ..
            } => {
                let id = format!("w{window_number}");
                let name = self
                    .effective_window_name(session_index, window_id)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                (id, name)
            }
            TreeRowKind::Pane {
                session_index,
                pane_id,
            } => {
                let id = format!("p{pane_id}");
                let name = self
                    .effective_pane_name(session_index, pane_id)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                (id, name)
            }
        }
    }

    pub(super) fn tree_row_search_key(&self, row: &TreeRow) -> String {
        let (id, name) = self.tree_row_id_and_name(row);
        if name.is_empty() {
            id
        } else {
            format!("{id} {name}")
        }
    }

    pub(super) fn tree_preview_pane_id(&self, row: &TreeRow) -> Option<(usize, usize)> {
        match row.kind {
            TreeRowKind::Session { session_index } => {
                let pane_id = self
                    .sessions
                    .get(session_index)?
                    .session
                    .focused_pane_id()?;
                Some((session_index, pane_id))
            }
            TreeRowKind::Window {
                session_index,
                window_id,
                ..
            } => {
                let pane_id = self
                    .sessions
                    .get(session_index)?
                    .session
                    .window_entries()
                    .into_iter()
                    .find(|entry| entry.window_id == window_id)
                    .map(|entry| entry.pane_id)?;
                Some((session_index, pane_id))
            }
            TreeRowKind::Pane {
                session_index,
                pane_id,
            } => Some((session_index, pane_id)),
        }
    }

    pub(super) fn tree_preview_lines(&self, row: &TreeRow) -> Vec<String> {
        let Some((session_index, pane_id)) = self.tree_preview_pane_id(row) else {
            return vec![TREE_PREVIEW_EMPTY.to_string()];
        };
        let Some(mut lines) = self.sessions.get(session_index).and_then(|managed| {
            managed
                .session
                .pane_history_tail_lines(pane_id, TREE_PREVIEW_MAX_LINES)
        }) else {
            return vec![TREE_PREVIEW_EMPTY.to_string()];
        };

        while matches!(lines.last(), Some(line) if line.trim().is_empty()) {
            let _ = lines.pop();
        }
        if lines.is_empty() {
            return vec![TREE_PREVIEW_EMPTY.to_string()];
        }
        lines
    }

    pub(super) fn apply_tree_selection(&mut self, row: &TreeRow) {
        match row.kind {
            TreeRowKind::Session { session_index } => {
                self.view.active_session = session_index;
                self.restore_focus_for_active_session_from_history();
                self.needs_full_clear = true;
            }
            TreeRowKind::Window {
                session_index,
                window_number,
                ..
            } => {
                self.view.active_session = session_index;
                self.restore_focus_for_active_session_from_history();
                if self
                    .current_session_mut()
                    .focus_window_number(window_number)
                    .is_ok()
                {
                    self.record_focus_for_active_session();
                }
                self.needs_full_clear = true;
            }
            TreeRowKind::Pane {
                session_index,
                pane_id,
            } => {
                self.view.active_session = session_index;
                if self.current_session_mut().focus_pane_id(pane_id).is_ok() {
                    self.record_focus_for_active_session();
                }
                self.needs_full_clear = true;
            }
        }
    }

    pub(super) fn execute_tree_delete(&mut self, target: TreeRowKind) -> Result<String, String> {
        let (cols, rows) = self.current_effective_pane_dims();
        match target {
            TreeRowKind::Session { session_index } => {
                let shutdown = self.kill_session_by_index(session_index)?;
                if shutdown {
                    Ok("killed final session; shutting down".to_string())
                } else {
                    self.sync_tree_names();
                    self.needs_full_clear = true;
                    Ok("session killed".to_string())
                }
            }
            TreeRowKind::Window {
                session_index,
                window_index,
                ..
            } => {
                self.sessions[session_index]
                    .session
                    .close_window(window_index, cols, rows)?;
                self.sync_tree_names();
                self.needs_full_clear = true;
                self.persist_active_session_info();
                Ok("window closed".to_string())
            }
            TreeRowKind::Pane {
                session_index,
                pane_id,
            } => {
                self.sessions[session_index]
                    .session
                    .close_pane(pane_id, cols, rows)?;
                self.sync_focus_history_for_active_session();
                self.sync_tree_names();
                self.needs_full_clear = true;
                self.persist_active_session_info();
                Ok("pane closed".to_string())
            }
        }
    }

    pub(super) fn tree_move_left(state: &mut SystemTreeState, rows: &[TreeRow]) {
        let Some(row) = rows.get(state.cursor_row) else {
            return;
        };

        match row.kind {
            TreeRowKind::Session { session_index } => {
                if row.expanded {
                    state.expanded_sessions.remove(&session_index);
                }
            }
            TreeRowKind::Window {
                session_index,
                window_index,
                ..
            } => {
                let key = TreeWindowKey {
                    session_index,
                    window_index,
                };
                if row.expanded {
                    state.expanded_windows.remove(&key);
                } else if let Some(parent) = row.parent_row {
                    state.cursor_row = parent;
                }
            }
            TreeRowKind::Pane { .. } => {
                if let Some(parent) = row.parent_row {
                    state.cursor_row = parent;
                }
            }
        }
    }

    pub(super) fn tree_move_right(state: &mut SystemTreeState, rows: &[TreeRow]) {
        let Some(row) = rows.get(state.cursor_row) else {
            return;
        };

        match row.kind {
            TreeRowKind::Session { session_index } => {
                if row.has_children && !row.expanded {
                    state.expanded_sessions.insert(session_index);
                    return;
                }
            }
            TreeRowKind::Window {
                session_index,
                window_index,
                ..
            } => {
                if row.has_children && !row.expanded {
                    state.expanded_windows.insert(TreeWindowKey {
                        session_index,
                        window_index,
                    });
                    return;
                }
            }
            TreeRowKind::Pane { .. } => return,
        }

        if let Some(next_row) = rows.get(state.cursor_row + 1)
            && next_row.parent_row == Some(state.cursor_row)
        {
            state.cursor_row += 1;
        }
    }
}

pub(super) fn tree_marker(has_children: bool, expanded: bool) -> char {
    if !has_children {
        ' '
    } else if expanded {
        '-'
    } else {
        '+'
    }
}

pub(super) fn rename_target_label(target: RenameTarget) -> &'static str {
    match target {
        RenameTarget::Session { .. } => "session",
        RenameTarget::Window { .. } => "window",
        RenameTarget::Pane { .. } => "pane",
    }
}

pub(super) fn named_tree_id(prefix: &str, id: usize, name: Option<&str>) -> String {
    if let Some(name) = name
        && !name.is_empty()
    {
        return format!("{prefix}{id}:{name}");
    }
    format!("{prefix}{id}")
}
