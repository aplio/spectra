use super::*;

impl TerminalGrid {
    pub(super) fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return;
        }

        // Reflow and the naive alt-screen resize treat `cells` as a linear
        // top-to-bottom buffer, so undo any ring rotation first.
        self.normalize_ring();

        if self.saved_screen.is_some() {
            self.resize_alt_screen_naive(width, height);
            if let Some(ref mut saved) = self.saved_screen {
                Self::reflow_saved_screen(saved, width, height);
            }
            return;
        }

        self.reflow_primary(width, height);
    }

    fn resize_alt_screen_naive(&mut self, new_width: usize, new_height: usize) {
        let old_width = self.width;
        let old_height = self.height;
        let old_cells = std::mem::take(&mut self.cells);
        let old_boundaries = std::mem::take(&mut self.row_boundaries);

        self.width = new_width;
        self.height = new_height;
        self.cells = vec![StyledCell::default(); new_width * new_height];
        self.row_boundaries = vec![RowBoundary::None; new_height];
        self.scroll_top = 0;
        self.scroll_bottom = new_height.saturating_sub(1);

        let copy_w = old_width.min(new_width);
        let copy_h = old_height.min(new_height);
        for y in 0..copy_h {
            for x in 0..copy_w {
                self.cells[y * new_width + x] = old_cells[y * old_width + x].clone();
            }
            // Shrinking can cut a wide char in half at the copy boundary:
            // blank an owner whose continuation cell no longer fits.
            if copy_w > 0 && copy_w < old_width {
                let idx = y * new_width + copy_w - 1;
                if UnicodeWidthChar::width(self.cells[idx].ch).unwrap_or(1) == 2 {
                    self.cells[idx] = StyledCell::default();
                }
            }
            self.row_boundaries[y] = old_boundaries.get(y).copied().unwrap_or(RowBoundary::None);
        }

        self.cursor_x = self.cursor_x.min(new_width.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(new_height.saturating_sub(1));
    }

    fn reflow_saved_screen(saved: &mut SavedScreen, new_width: usize, new_height: usize) {
        let old_height = saved.row_boundaries.len().max(1);
        let old_width = if saved.cells.is_empty() {
            1
        } else {
            saved.cells.len() / old_height
        };

        let mut temp = TerminalGrid::new(old_width, old_height, false);
        temp.cells = std::mem::take(&mut saved.cells);
        temp.scrollback = std::mem::take(&mut saved.scrollback);
        temp.row_boundaries = std::mem::take(&mut saved.row_boundaries);
        temp.cursor_x = saved.cursor_x;
        temp.cursor_y = saved.cursor_y;
        temp.active_style = saved.active_style;
        temp.scroll_top = saved.scroll_top;
        temp.scroll_bottom = saved.scroll_bottom;

        temp.reflow_primary(new_width, new_height);

        saved.cells = temp.cells;
        saved.scrollback = temp.scrollback;
        saved.row_boundaries = temp.row_boundaries;
        saved.cursor_x = temp.cursor_x;
        saved.cursor_y = temp.cursor_y;
        saved.active_style = temp.active_style;
        saved.scroll_top = temp.scroll_top;
        saved.scroll_bottom = temp.scroll_bottom;
    }

    fn reflow_primary(&mut self, new_width: usize, new_height: usize) {
        let (cursor_line_idx, cursor_col_offset) = self.cursor_in_logical_lines();
        let logical_lines = self.collect_logical_lines();

        let mut all_rows: Vec<(Vec<StyledCell>, RowBoundary)> = Vec::new();
        let mut cursor_abs_row = 0usize;
        let mut cursor_col = 0usize;

        for (line_idx, line) in logical_lines.iter().enumerate() {
            let line_rows = Self::rewrap_logical_line(line, new_width);
            if line_idx == cursor_line_idx {
                let (row_in_line, col) =
                    Self::map_offset_in_rewrap(&line.cells, cursor_col_offset, new_width);
                cursor_abs_row =
                    all_rows.len() + row_in_line.min(line_rows.len().saturating_sub(1));
                cursor_col = col;
            }
            all_rows.extend(line_rows);
        }

        // Strip trailing blank rows (any boundary — blank SoftWrap rows from
        // rewrapping empty logical lines are also meaningless)
        while all_rows.len() > 1
            && all_rows
                .last()
                .is_some_and(|(cells, _)| cells.iter().all(|c| *c == StyledCell::default()))
        {
            all_rows.pop();
        }

        // Ensure at least new_height rows
        while all_rows.len() < new_height {
            all_rows.push((vec![StyledCell::default(); new_width], RowBoundary::None));
        }

        let total = all_rows.len();
        let visible_start = total.saturating_sub(new_height);

        // Build scrollback
        let mut new_scrollback: Vec<HistoryLine> = Vec::new();
        for (cells, boundary) in &all_rows[..visible_start] {
            let text = cells
                .iter()
                .filter(|c| c.ch != '\0')
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_string();
            new_scrollback.push(HistoryLine {
                text,
                cells: cells.clone(),
                boundary_to_next: *boundary,
            });
        }
        let overflow = new_scrollback.len().saturating_sub(MAX_SCROLLBACK_LINES);
        if overflow > 0 {
            new_scrollback.drain(0..overflow);
        }

        // Build visible grid
        let mut new_cells = vec![StyledCell::default(); new_width * new_height];
        let mut new_boundaries = vec![RowBoundary::None; new_height];
        for (row_idx, (cells, boundary)) in all_rows[visible_start..].iter().enumerate() {
            let dst = row_idx * new_width;
            let len = cells.len().min(new_width);
            new_cells[dst..dst + len].clone_from_slice(&cells[..len]);
            new_boundaries[row_idx] = *boundary;
        }

        // Adjust cursor if it was in a stripped trailing row
        let cursor_abs_row = cursor_abs_row.min(total.saturating_sub(1));
        let cursor_y = cursor_abs_row.saturating_sub(visible_start);

        self.width = new_width;
        self.height = new_height;
        self.cells = new_cells;
        self.row0 = 0;
        self.row_boundaries = new_boundaries;
        self.scrollback = new_scrollback.into();
        self.scroll_top = 0;
        self.scroll_bottom = new_height.saturating_sub(1);
        self.cursor_x = cursor_col.min(new_width.saturating_sub(1));
        self.cursor_y = cursor_y.min(new_height.saturating_sub(1));
        self.saved_cursor_x = self.saved_cursor_x.min(new_width.saturating_sub(1));
        self.saved_cursor_y = self.saved_cursor_y.min(new_height.saturating_sub(1));
    }

    fn collect_logical_lines(&self) -> Vec<LogicalLine> {
        let mut lines: Vec<LogicalLine> = Vec::new();
        let mut current_cells: Vec<StyledCell> = Vec::new();

        for hist in &self.scrollback {
            current_cells.extend_from_slice(&hist.cells);
            if hist.boundary_to_next != RowBoundary::SoftWrap {
                trim_trailing_default_cells(&mut current_cells);
                lines.push(LogicalLine {
                    cells: std::mem::take(&mut current_cells),
                    trailing_boundary: hist.boundary_to_next,
                });
            }
        }

        for row in 0..self.height {
            let start = row * self.width;
            let end = start + self.width;
            current_cells.extend_from_slice(&self.cells[start..end]);
            let boundary = self.row_boundary_to_next(row);
            if boundary != RowBoundary::SoftWrap {
                trim_trailing_default_cells(&mut current_cells);
                lines.push(LogicalLine {
                    cells: std::mem::take(&mut current_cells),
                    trailing_boundary: boundary,
                });
            }
        }

        if !current_cells.is_empty() {
            trim_trailing_default_cells(&mut current_cells);
            lines.push(LogicalLine {
                cells: current_cells,
                trailing_boundary: RowBoundary::None,
            });
        }

        lines
    }

    /// Returns (logical_line_index, column_offset_within_that_line).
    /// Column offset is the cell index into the logical line's concatenated cells.
    fn cursor_in_logical_lines(&self) -> (usize, usize) {
        let cursor_col = self.cursor_x.min(self.width.saturating_sub(1));
        let mut line_idx = 0usize;
        let mut offset_in_line = 0usize;

        for hist in &self.scrollback {
            offset_in_line += hist.cells.len();
            if hist.boundary_to_next != RowBoundary::SoftWrap {
                line_idx += 1;
                offset_in_line = 0;
            }
        }

        for row in 0..self.height {
            if row == self.cursor_y {
                return (line_idx, offset_in_line + cursor_col);
            }
            offset_in_line += self.width;
            let boundary = self.row_boundary_to_next(row);
            if boundary != RowBoundary::SoftWrap {
                line_idx += 1;
                offset_in_line = 0;
            }
        }

        (line_idx, offset_in_line + cursor_col)
    }

    /// Given the cells of a logical line and a cell-index offset, find where
    /// that offset lands after rewrapping to `new_width`.
    /// Returns (row_within_line, column).
    fn map_offset_in_rewrap(
        cells: &[StyledCell],
        target: usize,
        new_width: usize,
    ) -> (usize, usize) {
        let mut row = 0usize;
        let mut col = 0usize;
        let mut i = 0usize;

        while i < cells.len() {
            if i == target {
                return (row, col);
            }

            let cell = &cells[i];
            if cell.ch == '\0' {
                // Continuation cell — skip in rewrap, but still a valid target
                i += 1;
                continue;
            }

            let ch_width = UnicodeWidthChar::width(cell.ch).unwrap_or(1).max(1);

            if ch_width == 2 {
                if col + 2 > new_width {
                    row += 1;
                    col = 0;
                }
                col += 2;
                i += 1;
            } else {
                if col >= new_width {
                    row += 1;
                    col = 0;
                }
                col += 1;
                i += 1;
            }
        }

        // Target at or past end
        if col >= new_width {
            row += 1;
            col = 0;
        }
        (row, col.min(new_width.saturating_sub(1)))
    }

    fn rewrap_logical_line(
        line: &LogicalLine,
        new_width: usize,
    ) -> Vec<(Vec<StyledCell>, RowBoundary)> {
        if line.cells.is_empty() {
            return vec![(
                vec![StyledCell::default(); new_width],
                line.trailing_boundary,
            )];
        }

        let mut rows: Vec<(Vec<StyledCell>, RowBoundary)> = Vec::new();
        let mut current_row = Vec::with_capacity(new_width);
        let mut col = 0usize;
        let mut i = 0usize;

        while i < line.cells.len() {
            let cell = line.cells[i].clone();

            // Skip continuation cells — we regenerate them
            if cell.ch == '\0' {
                i += 1;
                continue;
            }

            let ch_width = UnicodeWidthChar::width(cell.ch).unwrap_or(1).max(1);

            if ch_width == 2 {
                if col + 2 > new_width {
                    // Wide char doesn't fit — pad and wrap
                    while current_row.len() < new_width {
                        current_row.push(StyledCell::default());
                    }
                    rows.push((current_row, RowBoundary::SoftWrap));
                    current_row = Vec::with_capacity(new_width);
                    col = 0;
                }
                let continuation = StyledCell {
                    ch: '\0',
                    style: cell.style,
                    link: cell.link.clone(),
                };
                current_row.push(cell);
                current_row.push(continuation);
                col += 2;
                i += 1;
            } else {
                if col >= new_width {
                    while current_row.len() < new_width {
                        current_row.push(StyledCell::default());
                    }
                    rows.push((current_row, RowBoundary::SoftWrap));
                    current_row = Vec::with_capacity(new_width);
                    col = 0;
                }
                current_row.push(cell);
                col += 1;
                i += 1;
            }
        }

        // Pad the last row
        while current_row.len() < new_width {
            current_row.push(StyledCell::default());
        }
        rows.push((current_row, line.trailing_boundary));

        rows
    }

    pub(super) fn enter_alternate_screen(&mut self) {
        if self.saved_screen.is_some() {
            return;
        }
        // The saved snapshot is stored (and later restored) as a linear
        // buffer, so undo any ring rotation before stashing it.
        self.normalize_ring();
        self.saved_screen = Some(SavedScreen {
            cells: std::mem::replace(
                &mut self.cells,
                vec![StyledCell::default(); self.width * self.height],
            ),
            scrollback: std::mem::take(&mut self.scrollback),
            row_boundaries: std::mem::replace(
                &mut self.row_boundaries,
                vec![RowBoundary::None; self.height],
            ),
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            active_style: self.active_style,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
        });
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.active_style = CellStyle::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.height.saturating_sub(1);
    }

    pub(super) fn leave_alternate_screen(&mut self) {
        let Some(saved) = self.saved_screen.take() else {
            return;
        };
        self.cells = saved.cells;
        // The snapshot was normalized when saved; the alt screen's own
        // rotation dies with its buffer.
        self.row0 = 0;
        self.scrollback = saved.scrollback;
        self.row_boundaries = saved.row_boundaries;
        self.cursor_x = saved.cursor_x;
        self.cursor_y = saved.cursor_y;
        self.active_style = saved.active_style;
        self.scroll_top = saved.scroll_top;
        self.scroll_bottom = saved.scroll_bottom;
    }
}
