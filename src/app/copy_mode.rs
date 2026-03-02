use std::cmp::Ordering;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::App;
use super::types::*;
use crate::session::terminal_state::StyledCell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorModeWordClass {
    Word,
    Whitespace,
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
        let has_ctrl_or_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

        match key.code {
            KeyCode::Esc => return InputMode::Normal,
            KeyCode::Char('q') if !has_ctrl_or_alt => return InputMode::Normal,
            KeyCode::Char('h') | KeyCode::Left if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                Self::cursor_mode_move_left(&mut state);
            }
            KeyCode::Char('l') | KeyCode::Right if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                Self::cursor_mode_move_right(&mut state);
            }
            KeyCode::Char('j') | KeyCode::Down if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                Self::cursor_mode_move_vertical(&mut state, 1);
            }
            KeyCode::Char('k') | KeyCode::Up if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                Self::cursor_mode_move_vertical(&mut state, -1);
            }
            KeyCode::PageUp if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                let jump = view_rows.saturating_sub(1).max(1) as isize;
                Self::cursor_mode_move_vertical(&mut state, -jump);
            }
            KeyCode::PageDown if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                let jump = view_rows.saturating_sub(1).max(1) as isize;
                Self::cursor_mode_move_vertical(&mut state, jump);
            }
            KeyCode::Char('0') if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                state.cursor.col = 0;
            }
            KeyCode::Char('$') if !has_ctrl_or_alt => {
                state.selection_anchor = None;
                let len = Self::cursor_mode_line_char_len(&state, state.cursor.line);
                state.cursor.col = len.saturating_sub(1);
            }
            KeyCode::Char('w') if !has_ctrl_or_alt => {
                state.selection_anchor = Some(state.cursor);
                state.cursor = Self::cursor_mode_word_forward_point(&state, state.cursor);
            }
            KeyCode::Char('b') if !has_ctrl_or_alt => {
                state.selection_anchor = Some(state.cursor);
                state.cursor = Self::cursor_mode_word_backward_point(&state, state.cursor);
            }
            KeyCode::Char('e') if !has_ctrl_or_alt => {
                state.selection_anchor = Some(state.cursor);
                state.cursor = Self::cursor_mode_word_end_point(&state, state.cursor);
            }
            KeyCode::Char('v') if !has_ctrl_or_alt => {
                if state.selection_anchor == Some(state.cursor) {
                    state.selection_anchor = None;
                } else {
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
                self.cursor_mode_copy_selection(&state);
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

        let view_rows = focused_pane.rect.height.max(1);
        let viewport_top = self
            .current_session()
            .focused_view_row_origin(view_rows)
            .unwrap_or_else(|| lines.len().saturating_sub(view_rows));
        let (cursor_col, cursor_line) = self
            .current_session()
            .focused_cursor_absolute_position()
            .unwrap_or((0, lines.len().saturating_sub(1)));

        let mut state = CursorModeState {
            pane_id: focused_pane.pane_id,
            lines,
            styled_lines,
            cursor: CursorModePoint {
                line: cursor_line,
                col: cursor_col,
            },
            selection_anchor: None,
            viewport_top,
        };
        Self::cursor_mode_clamp_cursor(&mut state);
        Self::cursor_mode_ensure_visible(&mut state, view_rows);

        self.view.text_selection = None;
        self.view.mouse_drag = None;
        self.view.input_mode = InputMode::CursorMode { state };
    }

    pub(super) fn cursor_mode_selected_text(state: &CursorModeState) -> String {
        if state.lines.is_empty() {
            return String::new();
        }

        let mut cursor = state.cursor;
        Self::cursor_mode_clamp_point(state, &mut cursor);
        let Some(mut anchor) = state.selection_anchor else {
            return state.lines.get(cursor.line).cloned().unwrap_or_default();
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

        let mut parts = Vec::new();
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
            parts.push(text);
        }
        parts.join("\n")
    }

    fn cursor_mode_copy_selection(&mut self, state: &CursorModeState) {
        let text = Self::cursor_mode_selected_text(state);
        if text.is_empty() {
            self.set_message("cursor mode: nothing to copy", Duration::from_secs(2));
            return;
        }
        match self.copy_text_for_active_client(&text) {
            Ok(()) => self.set_message("copied to clipboard", Duration::from_secs(2)),
            Err(err) => self.set_message(
                &format!("clipboard copy failed: {err}"),
                Duration::from_secs(3),
            ),
        }
    }

    fn cursor_mode_is_word_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }

    fn cursor_mode_word_class(ch: char) -> CursorModeWordClass {
        if Self::cursor_mode_is_word_char(ch) {
            CursorModeWordClass::Word
        } else if ch.is_whitespace() {
            CursorModeWordClass::Whitespace
        } else {
            CursorModeWordClass::Other
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
    }

    fn cursor_mode_line_to_cells(line: &[StyledCell], width: usize) -> Vec<StyledCell> {
        if width == 0 {
            return Vec::new();
        }

        let mut cells = Vec::with_capacity(width);
        let mut index = 0usize;
        while index < line.len() && cells.len() < width {
            let cell = line[index];
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
                let Some(continuation) = line.get(index + 1).copied() else {
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
