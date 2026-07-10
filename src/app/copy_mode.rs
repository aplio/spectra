use std::cmp::Ordering;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;

use super::App;
use super::types::*;
use crate::input::text_input::TextInput;
use crate::session::terminal_state::StyledCell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorModeWordClass {
    Word,
    Whitespace,
    /// Hiragana run: word motions stop at kana/kanji transitions (vim-like).
    Hiragana,
    /// Katakana run, including halfwidth katakana and the prolonged sound
    /// mark, so ラーメン is one word.
    Katakana,
    /// Remaining double-width chars: CJK ideographs, hangul, full-width
    /// forms.
    Wide,
    Other,
}

impl App {
    pub(super) fn handle_cursor_mode_key(
        &mut self,
        mut state: CursorModeState,
        key: KeyEvent,
    ) -> InputMode {
        if state.lines.is_empty() {
            return InputMode::Normal;
        }

        let view_rows = self.cursor_mode_view_rows(&state);

        if state.search.input.is_some() {
            return self.handle_cursor_mode_search_key(state, key, view_rows);
        }

        let has_ctrl_or_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

        // Resolve a pending `g` chord (`gg`, `ge`, `gh`, `gl`). Any other key
        // simply cancels the chord and is otherwise ignored, mirroring gargo.
        if std::mem::take(&mut state.pending_goto) && !has_ctrl_or_alt {
            match key.code {
                KeyCode::Char('g') => {
                    Self::cursor_mode_drop_transient_selection(&mut state);
                    Self::cursor_mode_goto_line(&mut state, 0);
                }
                KeyCode::Char('e') => {
                    Self::cursor_mode_drop_transient_selection(&mut state);
                    let last = state.lines.len().saturating_sub(1);
                    Self::cursor_mode_goto_line(&mut state, last);
                }
                KeyCode::Char('h') => {
                    Self::cursor_mode_drop_transient_selection(&mut state);
                    state.cursor.col = 0;
                }
                KeyCode::Char('l') => {
                    Self::cursor_mode_drop_transient_selection(&mut state);
                    let len = Self::cursor_mode_line_char_len(&state, state.cursor.line);
                    state.cursor.col = len.saturating_sub(1);
                }
                _ => {}
            }
            Self::cursor_mode_clamp_cursor(&mut state);
            Self::cursor_mode_ensure_visible(&mut state, view_rows);
            return InputMode::CursorMode { state };
        }

        match key.code {
            KeyCode::Esc => return InputMode::Normal,
            KeyCode::Char('q') if !has_ctrl_or_alt => return InputMode::Normal,
            KeyCode::Char('h') | KeyCode::Left if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                Self::cursor_mode_move_left(&mut state);
            }
            KeyCode::Char('l') | KeyCode::Right if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                Self::cursor_mode_move_right(&mut state);
            }
            KeyCode::Char('j') | KeyCode::Down if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                Self::cursor_mode_move_vertical(&mut state, 1);
            }
            KeyCode::Char('k') | KeyCode::Up if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                Self::cursor_mode_move_vertical(&mut state, -1);
            }
            KeyCode::PageUp if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                let jump = view_rows.saturating_sub(1).max(1) as isize;
                Self::cursor_mode_move_vertical(&mut state, -jump);
            }
            KeyCode::PageDown if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                let jump = view_rows.saturating_sub(1).max(1) as isize;
                Self::cursor_mode_move_vertical(&mut state, jump);
            }
            KeyCode::Char('0') if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                state.cursor.col = 0;
            }
            KeyCode::Char('$') if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                let len = Self::cursor_mode_line_char_len(&state, state.cursor.line);
                state.cursor.col = len.saturating_sub(1);
            }
            KeyCode::Char('g') if !has_ctrl_or_alt => {
                state.pending_goto = true;
            }
            KeyCode::Char('G') if !has_ctrl_or_alt => {
                Self::cursor_mode_drop_transient_selection(&mut state);
                let last = state.lines.len().saturating_sub(1);
                Self::cursor_mode_goto_line(&mut state, last);
            }
            KeyCode::Char('w') if !has_ctrl_or_alt => {
                if !state.visual {
                    state.selection_anchor = Some(state.cursor);
                }
                state.cursor = Self::cursor_mode_word_forward_point(&state, state.cursor);
            }
            KeyCode::Char('b') if !has_ctrl_or_alt => {
                if !state.visual {
                    state.selection_anchor = Some(state.cursor);
                }
                state.cursor = Self::cursor_mode_word_backward_point(&state, state.cursor);
            }
            KeyCode::Char('e') if !has_ctrl_or_alt => {
                if !state.visual {
                    state.selection_anchor = Some(state.cursor);
                }
                state.cursor = Self::cursor_mode_word_end_point(&state, state.cursor);
            }
            KeyCode::Char('v') if !has_ctrl_or_alt => {
                if state.visual {
                    state.visual = false;
                    state.selection_anchor = None;
                } else {
                    state.visual = true;
                    state.selection_anchor = Some(state.cursor);
                }
            }
            KeyCode::Char('x') if !has_ctrl_or_alt => {
                if state.selection_anchor.is_some() {
                    Self::cursor_mode_extend_line_selection_down(&mut state);
                } else {
                    let line = state.cursor.line;
                    Self::cursor_mode_select_line(&mut state, line);
                }
            }
            KeyCode::Char('y') if !has_ctrl_or_alt => {
                if self.cursor_mode_copy_selection(&state) {
                    return InputMode::Normal;
                }
            }
            KeyCode::Char('/') if !has_ctrl_or_alt => {
                Self::cursor_mode_open_search_bar(&mut state);
            }
            KeyCode::Char('n') if !has_ctrl_or_alt => {
                self.cursor_mode_search_step(&mut state, true);
            }
            KeyCode::Char('N') if !has_ctrl_or_alt => {
                self.cursor_mode_search_step(&mut state, false);
            }
            KeyCode::Enter if !has_ctrl_or_alt => return InputMode::Normal,
            _ => {}
        }

        Self::cursor_mode_clamp_cursor(&mut state);
        Self::cursor_mode_ensure_visible(&mut state, view_rows);
        InputMode::CursorMode { state }
    }

    fn cursor_mode_view_rows(&self, state: &CursorModeState) -> usize {
        self.current_session()
            .frame(self.view.cols, self.view.rows)
            .panes
            .iter()
            .find(|pane| pane.pane_id == state.pane_id)
            .map(|pane| pane.rect.height.max(1))
            .unwrap_or_else(|| usize::from(self.view.rows.saturating_sub(1)).max(1))
    }

    fn cursor_mode_move_left(state: &mut CursorModeState) {
        state.cursor.col = state.cursor.col.saturating_sub(1);
    }

    fn cursor_mode_move_right(state: &mut CursorModeState) {
        let len = Self::cursor_mode_line_char_len(state, state.cursor.line);
        if len > 0 {
            state.cursor.col = (state.cursor.col + 1).min(len - 1);
        } else {
            state.cursor.col = 0;
        }
    }

    fn cursor_mode_move_vertical(state: &mut CursorModeState, delta: isize) {
        if state.lines.is_empty() {
            state.cursor = CursorModePoint::default();
            return;
        }

        let max_line = state.lines.len().saturating_sub(1);
        if delta.is_negative() {
            state.cursor.line = state.cursor.line.saturating_sub(delta.unsigned_abs());
        } else {
            state.cursor.line = state
                .cursor
                .line
                .saturating_add(delta as usize)
                .min(max_line);
        }

        let len = Self::cursor_mode_line_char_len(state, state.cursor.line);
        state.cursor.col = if len == 0 {
            0
        } else {
            state.cursor.col.min(len - 1)
        };
    }

    /// Jumps the cursor to `line` (clamped to the buffer), preserving the
    /// column where possible — the shared landing for `gg`/`ge`/`G`.
    fn cursor_mode_goto_line(state: &mut CursorModeState, line: usize) {
        if state.lines.is_empty() {
            state.cursor = CursorModePoint::default();
            return;
        }
        let max_line = state.lines.len().saturating_sub(1);
        state.cursor.line = line.min(max_line);
        let len = Self::cursor_mode_line_char_len(state, state.cursor.line);
        state.cursor.col = if len == 0 {
            0
        } else {
            state.cursor.col.min(len - 1)
        };
    }

    pub(super) fn cursor_mode_scroll_by(
        state: &mut CursorModeState,
        delta: isize,
        view_rows: usize,
    ) {
        Self::cursor_mode_move_vertical(state, delta);
        Self::cursor_mode_ensure_visible(state, view_rows);
    }

    pub(super) fn open_cursor_mode(&mut self) {
        let frame = self.current_session().frame(self.view.cols, self.view.rows);
        let Some(focused_pane) = frame.panes.iter().find(|pane| pane.focused) else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };

        let Some(mut lines) = self.current_session().focused_history_lines() else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };
        let Some(mut styled_lines) = self.current_session().focused_history_cells() else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };
        let mut soft_wraps = self
            .current_session()
            .focused_history_soft_wraps()
            .unwrap_or_default();
        if lines.is_empty() {
            lines.push(String::new());
        }
        if styled_lines.is_empty() {
            styled_lines.push(Vec::new());
        }
        if styled_lines.len() < lines.len() {
            styled_lines.resize_with(lines.len(), Vec::new);
        } else if styled_lines.len() > lines.len() {
            styled_lines.truncate(lines.len());
        }
        soft_wraps.resize(lines.len(), false);

        let view_rows = focused_pane.rect.height.max(1);
        let viewport_top = self
            .current_session()
            .focused_view_row_origin(view_rows)
            .unwrap_or_else(|| lines.len().saturating_sub(view_rows));
        let (cursor_cell_col, cursor_line) = self
            .current_session()
            .focused_cursor_absolute_position()
            .unwrap_or((0, lines.len().saturating_sub(1)));
        // The pane cursor is a display-cell column; cursor-mode points are
        // char indices, which differ as soon as wide (CJK) chars precede the
        // cursor on the row.
        let cursor_col = lines
            .get(cursor_line)
            .map(|line| Self::cursor_mode_cell_col_to_char_col(line, cursor_cell_col))
            .unwrap_or(cursor_cell_col);

        let mut state = CursorModeState {
            pane_id: focused_pane.pane_id,
            lines,
            styled_lines,
            soft_wraps,
            cursor: CursorModePoint {
                line: cursor_line,
                col: cursor_col,
            },
            selection_anchor: None,
            visual: false,
            viewport_top,
            pending_goto: false,
            search: CursorModeSearchState::default(),
        };
        Self::cursor_mode_clamp_cursor(&mut state);
        Self::cursor_mode_ensure_visible(&mut state, view_rows);

        self.view.text_selection = None;
        self.view.click_chain = None;
        self.view.mouse_drag = None;
        self.view.input_mode = InputMode::CursorMode { state };
    }

    /// Open cursor mode with the search bar already active (`prefix + /`).
    pub(super) fn open_cursor_mode_search(&mut self) {
        if !matches!(self.view.input_mode, InputMode::CursorMode { .. }) {
            self.open_cursor_mode();
        }
        if let InputMode::CursorMode { state } = &mut self.view.input_mode {
            Self::cursor_mode_open_search_bar(state);
        }
    }

    /// Arm the `/` search bar: capture the anchor the incremental search
    /// originates from and the cursor/viewport to restore on cancel. A
    /// previous pattern keeps highlighting until the first keystroke
    /// replaces it, mirroring gargo.
    fn cursor_mode_open_search_bar(state: &mut CursorModeState) {
        state.search.input = Some(TextInput::default());
        state.search.anchor = state.cursor;
        state.search.saved_cursor = state.cursor;
        state.search.saved_viewport_top = state.viewport_top;
        state.search.history_index = None;
        state.search.input_before_history.clear();
        state.pending_goto = false;
    }

    /// Keys while the search bar is open, mirroring gargo's search bar:
    /// printable chars search incrementally from the anchor, Enter confirms,
    /// Esc/Ctrl+q cancels and restores the pre-search view, Up/Down or
    /// Ctrl+p/n browse history, and emacs-style Ctrl editing works on the
    /// input.
    fn handle_cursor_mode_search_key(
        &mut self,
        mut state: CursorModeState,
        key: KeyEvent,
        view_rows: usize,
    ) -> InputMode {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => Self::cursor_mode_search_cancel(&mut state),
                KeyCode::Char('p') => self.cursor_mode_search_history_prev(&mut state),
                KeyCode::Char('n') => self.cursor_mode_search_history_next(&mut state),
                KeyCode::Char('a') => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_start();
                    false
                }),
                KeyCode::Char('e') => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_end();
                    false
                }),
                KeyCode::Char('f') => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_right();
                    false
                }),
                KeyCode::Char('b') => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_left();
                    false
                }),
                KeyCode::Char('k') => {
                    Self::cursor_mode_search_edit(&mut state, TextInput::delete_to_end)
                }
                KeyCode::Char('w') => {
                    Self::cursor_mode_search_edit(&mut state, TextInput::delete_prev_word)
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => Self::cursor_mode_search_cancel(&mut state),
                KeyCode::Enter => self.cursor_mode_search_confirm(&mut state),
                KeyCode::Up => self.cursor_mode_search_history_prev(&mut state),
                KeyCode::Down => self.cursor_mode_search_history_next(&mut state),
                KeyCode::Left => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_left();
                    false
                }),
                KeyCode::Right => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.move_right();
                    false
                }),
                KeyCode::Backspace => {
                    Self::cursor_mode_search_edit(&mut state, TextInput::backspace)
                }
                KeyCode::Char(ch) => Self::cursor_mode_search_edit(&mut state, |input| {
                    input.insert_char(ch);
                    true
                }),
                _ => {}
            }
        }

        Self::cursor_mode_clamp_cursor(&mut state);
        Self::cursor_mode_ensure_visible(&mut state, view_rows);
        InputMode::CursorMode { state }
    }

    /// Apply `edit` to the search input; a `true` return means the text
    /// changed and the incremental search re-runs from the anchor.
    fn cursor_mode_search_edit(
        state: &mut CursorModeState,
        edit: impl FnOnce(&mut TextInput) -> bool,
    ) {
        let Some(input) = state.search.input.as_mut() else {
            return;
        };
        if edit(input) {
            Self::cursor_mode_search_update(state);
        }
    }

    /// Paste into an open search bar (no-op while the bar is closed).
    pub(super) fn cursor_mode_search_paste(
        state: &mut CursorModeState,
        text: &str,
        view_rows: usize,
    ) {
        if state.search.input.is_none() {
            return;
        }
        Self::cursor_mode_search_edit(state, |input| input.insert_text(text));
        Self::cursor_mode_clamp_cursor(state);
        Self::cursor_mode_ensure_visible(state, view_rows);
    }

    /// Recompute the pattern from the bar input and jump to the first match
    /// at or after the anchor (wrapping), so editing the pattern never
    /// drifts the result through the buffer.
    fn cursor_mode_search_update(state: &mut CursorModeState) {
        state.search.pattern = state
            .search
            .input
            .as_ref()
            .map(|input| input.text.clone())
            .unwrap_or_default();
        state.search.pattern_lower = state
            .search
            .pattern
            .chars()
            .map(Self::cursor_mode_search_lower_char)
            .collect();
        state.search.last_found = false;
        if state.search.pattern_lower.is_empty() {
            return;
        }
        if let Some(hit) = Self::cursor_mode_search_find_forward(state, state.search.anchor) {
            Self::cursor_mode_drop_transient_selection(state);
            state.cursor = hit;
            state.search.last_found = true;
        }
    }

    /// Enter: close the bar keeping the pattern (and its highlights) live
    /// for `n`/`N`, and record it in the client's search history.
    fn cursor_mode_search_confirm(&mut self, state: &mut CursorModeState) {
        let typed = state
            .search
            .input
            .take()
            .map(|input| input.text)
            .unwrap_or_default();
        state.search.history_index = None;
        state.search.input_before_history.clear();
        if typed.is_empty() {
            return;
        }
        if self.view.search_history.last() != Some(&typed) {
            self.view.search_history.push(typed);
        }
        if !state.search.last_found {
            self.set_message("pattern not found", Duration::from_secs(2));
        }
    }

    /// Esc/Ctrl+q: drop the pattern and put the cursor and viewport back
    /// where they were when the bar opened.
    fn cursor_mode_search_cancel(state: &mut CursorModeState) {
        state.search.input = None;
        state.search.pattern.clear();
        state.search.pattern_lower.clear();
        state.search.last_found = false;
        state.search.history_index = None;
        state.search.input_before_history.clear();
        state.cursor = state.search.saved_cursor;
        state.viewport_top = state.search.saved_viewport_top;
    }

    /// `n`/`N`: step to the next/previous match, wrapping around the buffer.
    fn cursor_mode_search_step(&mut self, state: &mut CursorModeState, forward: bool) {
        if state.search.pattern_lower.is_empty() {
            self.set_message("no search pattern", Duration::from_secs(2));
            return;
        }
        let hit = if forward {
            let from = Self::cursor_mode_search_next_origin(state);
            Self::cursor_mode_search_find_forward(state, from)
        } else {
            Self::cursor_mode_search_find_backward(state, state.cursor)
        };
        match hit {
            Some(point) => {
                Self::cursor_mode_drop_transient_selection(state);
                state.cursor = point;
            }
            None => self.set_message("pattern not found", Duration::from_secs(2)),
        }
    }

    /// Where a forward `n` starts scanning: just past the match under the
    /// cursor, so consecutive presses step through non-overlapping hits.
    fn cursor_mode_search_next_origin(state: &CursorModeState) -> CursorModePoint {
        let pattern_len = state.search.pattern_lower.len().max(1);
        let line_len = Self::cursor_mode_line_char_len(state, state.cursor.line);
        let col = state.cursor.col.saturating_add(pattern_len);
        if col < line_len {
            CursorModePoint {
                line: state.cursor.line,
                col,
            }
        } else {
            CursorModePoint {
                line: state.cursor.line.saturating_add(1),
                col: 0,
            }
        }
    }

    fn cursor_mode_search_history_prev(&mut self, state: &mut CursorModeState) {
        if self.view.search_history.is_empty() {
            return;
        }
        let index = match state.search.history_index {
            None => {
                state.search.input_before_history = state
                    .search
                    .input
                    .as_ref()
                    .map(|input| input.text.clone())
                    .unwrap_or_default();
                self.view.search_history.len() - 1
            }
            Some(0) => return,
            Some(index) => index - 1,
        };
        state.search.history_index = Some(index);
        let pattern = self.view.search_history[index].clone();
        if let Some(input) = state.search.input.as_mut() {
            input.set_text(pattern);
        }
        Self::cursor_mode_search_update(state);
    }

    fn cursor_mode_search_history_next(&mut self, state: &mut CursorModeState) {
        let Some(index) = state.search.history_index else {
            return;
        };
        let text = if index + 1 < self.view.search_history.len() {
            state.search.history_index = Some(index + 1);
            self.view.search_history[index + 1].clone()
        } else {
            // Past the newest entry: back to what the user had typed.
            state.search.history_index = None;
            std::mem::take(&mut state.search.input_before_history)
        };
        if let Some(input) = state.search.input.as_mut() {
            input.set_text(text);
        }
        Self::cursor_mode_search_update(state);
    }

    /// Lowercase for matching, one char at a time so char indices stay 1:1
    /// with the original line (multi-char expansions like ß→ss keep only
    /// their first char).
    fn cursor_mode_search_lower_char(ch: char) -> char {
        ch.to_lowercase().next().unwrap_or(ch)
    }

    fn cursor_mode_search_line_lower(line: &str) -> Vec<char> {
        line.chars()
            .map(Self::cursor_mode_search_lower_char)
            .collect()
    }

    /// First match start in `line_lower` at or after `from_col`.
    fn cursor_mode_search_line_find_from(
        line_lower: &[char],
        pattern_lower: &[char],
        from_col: usize,
    ) -> Option<usize> {
        let len = line_lower.len();
        let pattern_len = pattern_lower.len();
        if pattern_len == 0 || len < pattern_len || from_col > len - pattern_len {
            return None;
        }
        (from_col..=len - pattern_len)
            .find(|&start| line_lower[start..start + pattern_len] == *pattern_lower)
    }

    /// Last match start in `line_lower` strictly before `before_col`.
    fn cursor_mode_search_line_rfind_before(
        line_lower: &[char],
        pattern_lower: &[char],
        before_col: usize,
    ) -> Option<usize> {
        let len = line_lower.len();
        let pattern_len = pattern_lower.len();
        if pattern_len == 0 || len < pattern_len {
            return None;
        }
        let upper = before_col.min(len - pattern_len + 1);
        (0..upper)
            .rev()
            .find(|&start| line_lower[start..start + pattern_len] == *pattern_lower)
    }

    /// First match at or after `from`, wrapping to the top of the buffer.
    /// Matches never span line boundaries (patterns are single-line).
    fn cursor_mode_search_find_forward(
        state: &CursorModeState,
        from: CursorModePoint,
    ) -> Option<CursorModePoint> {
        let pattern = &state.search.pattern_lower;
        if pattern.is_empty() || state.lines.is_empty() {
            return None;
        }
        let total = state.lines.len();
        for line_idx in from.line.min(total)..total {
            let lower = Self::cursor_mode_search_line_lower(&state.lines[line_idx]);
            let from_col = if line_idx == from.line { from.col } else { 0 };
            if let Some(col) = Self::cursor_mode_search_line_find_from(&lower, pattern, from_col) {
                return Some(CursorModePoint {
                    line: line_idx,
                    col,
                });
            }
        }
        // Wrap: the first pass covered everything at or after `from`, so any
        // hit down here is genuinely before it (or `from` itself again).
        for line_idx in 0..=from.line.min(total - 1) {
            let lower = Self::cursor_mode_search_line_lower(&state.lines[line_idx]);
            if let Some(col) = Self::cursor_mode_search_line_find_from(&lower, pattern, 0) {
                return Some(CursorModePoint {
                    line: line_idx,
                    col,
                });
            }
        }
        None
    }

    /// Last match strictly before `before`, wrapping to the buffer tail.
    fn cursor_mode_search_find_backward(
        state: &CursorModeState,
        before: CursorModePoint,
    ) -> Option<CursorModePoint> {
        let pattern = &state.search.pattern_lower;
        if pattern.is_empty() || state.lines.is_empty() {
            return None;
        }
        let total = state.lines.len();
        let start_line = before.line.min(total - 1);
        let lower = Self::cursor_mode_search_line_lower(&state.lines[start_line]);
        if let Some(col) = Self::cursor_mode_search_line_rfind_before(&lower, pattern, before.col) {
            return Some(CursorModePoint {
                line: start_line,
                col,
            });
        }
        for line_idx in (0..start_line).rev() {
            let lower = Self::cursor_mode_search_line_lower(&state.lines[line_idx]);
            if let Some(col) =
                Self::cursor_mode_search_line_rfind_before(&lower, pattern, usize::MAX)
            {
                return Some(CursorModePoint {
                    line: line_idx,
                    col,
                });
            }
        }
        // Wrap from the tail down. A hit on the cursor line here is at or
        // after the cursor (everything earlier was rejected above).
        for line_idx in (start_line..total).rev() {
            let lower = Self::cursor_mode_search_line_lower(&state.lines[line_idx]);
            if let Some(col) =
                Self::cursor_mode_search_line_rfind_before(&lower, pattern, usize::MAX)
            {
                return Some(CursorModePoint {
                    line: line_idx,
                    col,
                });
            }
        }
        None
    }

    /// Paint every visible match gargo-style: DarkYellow background for
    /// matches, Yellow-on-Black for the match under the cursor.
    fn cursor_mode_apply_search_highlight(
        rows: &mut [Vec<StyledCell>],
        state: &CursorModeState,
        width: usize,
        height: usize,
    ) {
        let pattern = &state.search.pattern_lower;
        if pattern.is_empty() || width == 0 {
            return;
        }
        let pattern_len = pattern.len();
        for (row, cells) in rows.iter_mut().enumerate().take(height) {
            let line_idx = state.viewport_top.saturating_add(row);
            let Some(line) = state.lines.get(line_idx) else {
                continue;
            };
            let lower = Self::cursor_mode_search_line_lower(line);
            let mut scan_from = 0usize;
            while let Some(col) =
                Self::cursor_mode_search_line_find_from(&lower, pattern, scan_from)
            {
                let last_col = col + pattern_len - 1;
                let start_cell = Self::cursor_mode_char_col_to_cell_col(line, col, width);
                let end_cell = Self::cursor_mode_char_col_to_cell_col_end(line, last_col, width);
                let current = line_idx == state.cursor.line && col == state.cursor.col;
                for cell_idx in start_cell..=end_cell {
                    if let Some(cell) = cells.get_mut(cell_idx) {
                        if current {
                            cell.style.bg = Some(Color::Yellow);
                            cell.style.fg = Some(Color::Black);
                        } else {
                            cell.style.bg = Some(Color::DarkYellow);
                        }
                    }
                }
                scan_from = col + pattern_len;
            }
        }
    }

    pub(super) fn cursor_mode_selected_text(state: &CursorModeState) -> String {
        if state.lines.is_empty() {
            return String::new();
        }

        let mut cursor = state.cursor;
        Self::cursor_mode_clamp_point(state, &mut cursor);
        let Some(mut anchor) = state.selection_anchor else {
            return Self::cursor_mode_logical_line(state, cursor.line);
        };
        Self::cursor_mode_clamp_point(state, &mut anchor);

        let (start, end) = Self::cursor_mode_ordered_points(anchor, cursor);
        if start.line == end.line {
            return Self::cursor_mode_slice_inclusive(
                state
                    .lines
                    .get(start.line)
                    .map(String::as_str)
                    .unwrap_or(""),
                start.col,
                end.col,
            );
        }

        let mut out = String::new();
        for line_idx in start.line..=end.line {
            let line = state.lines.get(line_idx).map(String::as_str).unwrap_or("");
            let text = if line_idx == start.line {
                let end_col = line.chars().count().saturating_sub(1);
                Self::cursor_mode_slice_inclusive(line, start.col, end_col)
            } else if line_idx == end.line {
                Self::cursor_mode_slice_inclusive(line, 0, end.col)
            } else {
                line.to_string()
            };
            out.push_str(&text);
            // A soft-wrapped row continues on the next one without a real
            // LF; only hard line ends contribute a newline to the copy.
            if line_idx < end.line && !state.soft_wraps.get(line_idx).copied().unwrap_or(false) {
                out.push('\n');
            }
        }
        out
    }

    /// The full logical line containing display row `line`: soft-wrapped
    /// fragments above and below are rejoined without separators, so a `y`
    /// on any row of a wrapped line copies the whole unwrapped line.
    fn cursor_mode_logical_line(state: &CursorModeState, line: usize) -> String {
        let mut start = line;
        while start > 0 && state.soft_wraps.get(start - 1).copied().unwrap_or(false) {
            start -= 1;
        }
        let mut out = String::new();
        for idx in start..state.lines.len() {
            out.push_str(state.lines.get(idx).map(String::as_str).unwrap_or(""));
            if !state.soft_wraps.get(idx).copied().unwrap_or(false) {
                break;
            }
        }
        out
    }

    /// Copies the current selection (or line) and returns `true` when the
    /// text made it to the clipboard.
    fn cursor_mode_copy_selection(&mut self, state: &CursorModeState) -> bool {
        let text = Self::cursor_mode_selected_text(state);
        if text.is_empty() {
            self.set_message("cursor mode: nothing to copy", Duration::from_secs(2));
            return false;
        }
        match self.copy_text_for_active_client(&text) {
            Ok(()) => {
                self.set_message("copied to clipboard", Duration::from_secs(2));
                true
            }
            Err(err) => {
                self.set_message(
                    &format!("clipboard copy failed: {err}"),
                    Duration::from_secs(3),
                );
                false
            }
        }
    }

    /// Drops a transient (word-motion) selection before plain movement; a
    /// `v` visual selection stays anchored and extends instead.
    fn cursor_mode_drop_transient_selection(state: &mut CursorModeState) {
        if !state.visual {
            state.selection_anchor = None;
        }
    }

    fn cursor_mode_is_word_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }

    pub(super) fn cursor_mode_word_class(ch: char) -> CursorModeWordClass {
        if Self::cursor_mode_is_word_char(ch) {
            CursorModeWordClass::Word
        } else if ch.is_whitespace() {
            CursorModeWordClass::Whitespace
        } else {
            match ch {
                '\u{3041}'..='\u{309F}' => CursorModeWordClass::Hiragana,
                '\u{30A1}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}' => {
                    CursorModeWordClass::Katakana
                }
                // CJK punctuation (、。「」 …) groups with ASCII punctuation.
                '\u{3000}'..='\u{303F}' => CursorModeWordClass::Other,
                _ if UnicodeWidthChar::width(ch) == Some(2) => CursorModeWordClass::Wide,
                _ => CursorModeWordClass::Other,
            }
        }
    }

    fn cursor_mode_is_inline_whitespace(ch: char) -> bool {
        ch.is_whitespace() && ch != '\n' && ch != '\r'
    }

    fn cursor_mode_point_char(state: &CursorModeState, point: CursorModePoint) -> Option<char> {
        state
            .lines
            .get(point.line)
            .and_then(|line| line.chars().nth(point.col))
    }

    fn cursor_mode_point_class(
        state: &CursorModeState,
        point: CursorModePoint,
    ) -> CursorModeWordClass {
        match Self::cursor_mode_point_char(state, point) {
            Some(ch) => Self::cursor_mode_word_class(ch),
            None => CursorModeWordClass::Whitespace,
        }
    }

    fn cursor_mode_next_point(
        state: &CursorModeState,
        point: CursorModePoint,
    ) -> Option<CursorModePoint> {
        if state.lines.is_empty() || point.line >= state.lines.len() {
            return None;
        }

        let line_len = Self::cursor_mode_line_char_len(state, point.line);
        if line_len == 0 {
            if point.line + 1 < state.lines.len() {
                return Some(CursorModePoint {
                    line: point.line + 1,
                    col: 0,
                });
            }
            return None;
        }

        if point.col + 1 < line_len {
            return Some(CursorModePoint {
                line: point.line,
                col: point.col + 1,
            });
        }
        if point.line + 1 < state.lines.len() {
            return Some(CursorModePoint {
                line: point.line + 1,
                col: 0,
            });
        }
        None
    }

    fn cursor_mode_prev_point(
        state: &CursorModeState,
        point: CursorModePoint,
    ) -> Option<CursorModePoint> {
        if state.lines.is_empty() || point.line >= state.lines.len() {
            return None;
        }

        let line_len = Self::cursor_mode_line_char_len(state, point.line);
        if line_len > 0 && point.col > 0 {
            return Some(CursorModePoint {
                line: point.line,
                col: point.col - 1,
            });
        }
        if point.line == 0 {
            return None;
        }

        let prev_line = point.line - 1;
        let prev_len = Self::cursor_mode_line_char_len(state, prev_line);
        Some(CursorModePoint {
            line: prev_line,
            col: prev_len.saturating_sub(1),
        })
    }

    fn cursor_mode_word_forward_point(
        state: &CursorModeState,
        mut point: CursorModePoint,
    ) -> CursorModePoint {
        Self::cursor_mode_clamp_point(state, &mut point);
        if state.lines.is_empty() {
            return point;
        }

        // Empty line behaves like a line-break boundary: advance first.
        if Self::cursor_mode_line_char_len(state, point.line) == 0 {
            let Some(next) = Self::cursor_mode_next_point(state, point) else {
                return point;
            };
            point = next;
        } else {
            let start_class = Self::cursor_mode_point_class(state, point);
            if start_class != CursorModeWordClass::Whitespace {
                while let Some(next) = Self::cursor_mode_next_point(state, point) {
                    if next.line != point.line {
                        break;
                    }
                    if Self::cursor_mode_point_class(state, next) == start_class {
                        point = next;
                    } else {
                        break;
                    }
                }
                let Some(next) = Self::cursor_mode_next_point(state, point) else {
                    return point;
                };
                point = next;
            }
        }

        while let Some(ch) = Self::cursor_mode_point_char(state, point) {
            if !Self::cursor_mode_is_inline_whitespace(ch) {
                break;
            }
            let Some(next) = Self::cursor_mode_next_point(state, point) else {
                break;
            };
            point = next;
        }
        point
    }

    fn cursor_mode_word_backward_point(
        state: &CursorModeState,
        mut point: CursorModePoint,
    ) -> CursorModePoint {
        Self::cursor_mode_clamp_point(state, &mut point);
        if state.lines.is_empty() {
            return point;
        }

        let Some(prev) = Self::cursor_mode_prev_point(state, point) else {
            return point;
        };
        point = prev;

        while Self::cursor_mode_point_class(state, point) == CursorModeWordClass::Whitespace {
            let Some(prev) = Self::cursor_mode_prev_point(state, point) else {
                return point;
            };
            point = prev;
        }

        let target_class = Self::cursor_mode_point_class(state, point);
        while let Some(prev) = Self::cursor_mode_prev_point(state, point) {
            if prev.line != point.line {
                break;
            }
            if Self::cursor_mode_point_class(state, prev) == target_class {
                point = prev;
            } else {
                break;
            }
        }
        point
    }

    fn cursor_mode_word_end_point(
        state: &CursorModeState,
        mut point: CursorModePoint,
    ) -> CursorModePoint {
        Self::cursor_mode_clamp_point(state, &mut point);
        if state.lines.is_empty() {
            return point;
        }

        let Some(next) = Self::cursor_mode_next_point(state, point) else {
            return point;
        };
        point = next;

        while Self::cursor_mode_point_class(state, point) == CursorModeWordClass::Whitespace {
            let Some(next) = Self::cursor_mode_next_point(state, point) else {
                return point;
            };
            point = next;
        }

        let target_class = Self::cursor_mode_point_class(state, point);
        while let Some(next) = Self::cursor_mode_next_point(state, point) {
            if Self::cursor_mode_point_class(state, next) == target_class {
                point = next;
            } else {
                break;
            }
        }
        point
    }

    fn cursor_mode_ordered_points(
        first: CursorModePoint,
        second: CursorModePoint,
    ) -> (CursorModePoint, CursorModePoint) {
        match (first.line.cmp(&second.line), first.col.cmp(&second.col)) {
            (Ordering::Less, _) => (first, second),
            (Ordering::Greater, _) => (second, first),
            (Ordering::Equal, Ordering::Less | Ordering::Equal) => (first, second),
            (Ordering::Equal, Ordering::Greater) => (second, first),
        }
    }

    fn cursor_mode_slice_inclusive(line: &str, from_col: usize, to_col: usize) -> String {
        let len = line.chars().count();
        if len == 0 {
            return String::new();
        }
        let start = from_col.min(len.saturating_sub(1));
        let end = to_col.min(len.saturating_sub(1));
        if start > end {
            return String::new();
        }
        line.chars().skip(start).take(end - start + 1).collect()
    }

    fn cursor_mode_line_char_len(state: &CursorModeState, line: usize) -> usize {
        state
            .lines
            .get(line)
            .map(|entry| entry.chars().count())
            .unwrap_or(0)
    }

    fn cursor_mode_line_end_col(state: &CursorModeState, line: usize) -> usize {
        Self::cursor_mode_line_char_len(state, line).saturating_sub(1)
    }

    fn cursor_mode_select_line(state: &mut CursorModeState, line: usize) {
        state.selection_anchor = Some(CursorModePoint { line, col: 0 });
        state.cursor = CursorModePoint {
            line,
            col: Self::cursor_mode_line_end_col(state, line),
        };
    }

    fn cursor_mode_extend_line_selection_down(state: &mut CursorModeState) {
        if state.lines.is_empty() {
            return;
        }
        let next_line = state
            .cursor
            .line
            .saturating_add(1)
            .min(state.lines.len().saturating_sub(1));
        state.cursor = CursorModePoint {
            line: next_line,
            col: Self::cursor_mode_line_end_col(state, next_line),
        };
    }

    fn cursor_mode_clamp_point(state: &CursorModeState, point: &mut CursorModePoint) {
        if state.lines.is_empty() {
            *point = CursorModePoint::default();
            return;
        }
        point.line = point.line.min(state.lines.len().saturating_sub(1));
        let line_len = Self::cursor_mode_line_char_len(state, point.line);
        point.col = if line_len == 0 {
            0
        } else {
            point.col.min(line_len.saturating_sub(1))
        };
    }

    fn cursor_mode_clamp_cursor(state: &mut CursorModeState) {
        let mut cursor = state.cursor;
        Self::cursor_mode_clamp_point(state, &mut cursor);
        state.cursor = cursor;
        if let Some(mut anchor) = state.selection_anchor {
            Self::cursor_mode_clamp_point(state, &mut anchor);
            state.selection_anchor = Some(anchor);
        }
    }

    fn cursor_mode_ensure_visible(state: &mut CursorModeState, view_rows: usize) {
        if view_rows == 0 || state.lines.is_empty() {
            state.viewport_top = 0;
            return;
        }

        let max_top = state.lines.len().saturating_sub(view_rows);
        if state.cursor.line < state.viewport_top {
            state.viewport_top = state.cursor.line;
        } else if state.cursor.line >= state.viewport_top.saturating_add(view_rows) {
            state.viewport_top = state
                .cursor
                .line
                .saturating_add(1)
                .saturating_sub(view_rows);
        }
        state.viewport_top = state.viewport_top.min(max_top);
    }

    pub(super) fn apply_cursor_mode_frame(
        frame: &mut crate::session::manager::RenderFrame,
        state: &CursorModeState,
    ) {
        let Some(pane) = frame
            .panes
            .iter_mut()
            .find(|pane| pane.pane_id == state.pane_id)
        else {
            return;
        };
        let width = pane.rect.width;
        let height = pane.rect.height;
        if width == 0 || height == 0 {
            return;
        }

        for row in 0..height {
            let line_idx = state.viewport_top.saturating_add(row);
            let line = state
                .styled_lines
                .get(line_idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let cells = Self::cursor_mode_line_to_cells(line, width);
            if let Some(target) = pane.rows.get_mut(row) {
                *target = cells;
            } else {
                pane.rows.push(cells);
            }
        }
        pane.rows.truncate(height);
        Self::cursor_mode_apply_frame_selection_highlight(&mut pane.rows, state, width, height);
        Self::cursor_mode_apply_search_highlight(&mut pane.rows, state, width, height);

        let line = state
            .lines
            .get(state.cursor.line)
            .map(String::as_str)
            .unwrap_or("");
        let cursor_col = Self::cursor_mode_char_col_to_cell_col(line, state.cursor.col, width);
        let cursor_row = if state.cursor.line < state.viewport_top {
            0
        } else {
            state
                .cursor
                .line
                .saturating_sub(state.viewport_top)
                .min(height.saturating_sub(1))
        };
        frame.focused_cursor = Some((
            (pane.rect.x.saturating_add(cursor_col)) as u16,
            (pane.rect.y.saturating_add(cursor_row)) as u16,
        ));
        // Cursor mode shows spectra's own cursor, independent of the guest's
        // DECTCEM state.
        frame.focused_cursor_hidden = false;
    }

    fn cursor_mode_line_to_cells(line: &[StyledCell], width: usize) -> Vec<StyledCell> {
        if width == 0 {
            return Vec::new();
        }

        let mut cells = Vec::with_capacity(width);
        let mut index = 0usize;
        while index < line.len() && cells.len() < width {
            let cell = line[index].clone();
            if cell.ch == '\0' {
                cells.push(StyledCell::default());
                index += 1;
                continue;
            }

            let char_width = UnicodeWidthChar::width(cell.ch).unwrap_or(1).max(1);
            if char_width == 2 {
                if cells.len() + 1 >= width {
                    break;
                }
                let Some(continuation) = line.get(index + 1).cloned() else {
                    break;
                };
                if continuation.ch != '\0' {
                    cells.push(StyledCell::default());
                    index += 1;
                    continue;
                }
                cells.push(cell);
                cells.push(continuation);
                index += 2;
                continue;
            }

            cells.push(cell);
            index += 1;
        }
        if cells.len() < width {
            cells.resize(width, StyledCell::default());
        }
        cells
    }

    fn cursor_mode_apply_frame_selection_highlight(
        rows: &mut [Vec<StyledCell>],
        state: &CursorModeState,
        width: usize,
        height: usize,
    ) {
        let Some(anchor) = state.selection_anchor else {
            return;
        };

        let (start, end) = Self::cursor_mode_ordered_points(anchor, state.cursor);
        let visible_start = state.viewport_top;
        let visible_end = visible_start.saturating_add(height.saturating_sub(1));
        let from_line = start.line.max(visible_start);
        let to_line = end.line.min(visible_end);
        if from_line > to_line {
            return;
        }

        for line_idx in from_line..=to_line {
            let row_idx = line_idx.saturating_sub(visible_start);
            let Some(cells) = rows.get_mut(row_idx) else {
                continue;
            };
            let line = state.lines.get(line_idx).map(String::as_str).unwrap_or("");
            if line.is_empty() {
                continue;
            }

            let line_len = line.chars().count().saturating_sub(1);
            let from_col = if line_idx == start.line { start.col } else { 0 };
            let to_col = if line_idx == end.line {
                end.col.min(line_len)
            } else {
                line_len
            };

            let start_cell = Self::cursor_mode_char_col_to_cell_col(line, from_col, width);
            let end_cell = Self::cursor_mode_char_col_to_cell_col_end(line, to_col, width);
            if start_cell > end_cell {
                continue;
            }
            for cell_idx in start_cell..=end_cell {
                if let Some(cell) = cells.get_mut(cell_idx) {
                    cell.style.reverse = !cell.style.reverse;
                }
            }
        }
    }

    /// Inverse of [`Self::cursor_mode_char_col_to_cell_col`]: map a pane
    /// display-cell column onto the index of the char occupying that cell.
    /// Columns past the text map to the char count (end of line).
    fn cursor_mode_cell_col_to_char_col(line: &str, cell_col: usize) -> usize {
        let mut cells = 0usize;
        let mut chars = 0usize;
        for ch in line.chars() {
            if cells >= cell_col {
                return chars;
            }
            cells += UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            // A cell column landing inside a wide char maps to that char.
            if cells > cell_col {
                return chars;
            }
            chars += 1;
        }
        chars
    }

    fn cursor_mode_char_col_to_cell_col(line: &str, col: usize, width: usize) -> usize {
        if width == 0 {
            return 0;
        }
        let mut cell_col = 0usize;
        for (idx, ch) in line.chars().enumerate() {
            if idx >= col {
                break;
            }
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            if cell_col + char_width >= width {
                return width.saturating_sub(1);
            }
            cell_col += char_width;
        }
        cell_col.min(width.saturating_sub(1))
    }

    fn cursor_mode_char_col_to_cell_col_end(line: &str, col: usize, width: usize) -> usize {
        if width == 0 {
            return 0;
        }
        let mut cell_col = 0usize;
        for (idx, ch) in line.chars().enumerate() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            if idx == col {
                return (cell_col + char_width.saturating_sub(1)).min(width.saturating_sub(1));
            }
            if cell_col + char_width >= width {
                return width.saturating_sub(1);
            }
            cell_col += char_width;
        }
        cell_col.min(width.saturating_sub(1))
    }
}
