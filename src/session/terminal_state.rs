use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

pub use crate::ui::style::CellStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledCell {
    pub ch: char,
    pub style: CellStyle,
}

impl Default for StyledCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: CellStyle::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RowBoundary {
    #[default]
    None,
    SoftWrap,
    HardLf,
}

#[derive(Debug, Clone)]
struct HistoryLine {
    text: String,
    cells: Vec<StyledCell>,
    boundary_to_next: RowBoundary,
}

struct LogicalLine {
    cells: Vec<StyledCell>,
    trailing_boundary: RowBoundary,
}

fn trim_trailing_default_cells(cells: &mut Vec<StyledCell>) {
    while let Some(last) = cells.last() {
        if *last == StyledCell::default() {
            cells.pop();
        } else {
            break;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    TitleChanged { title: Option<String> },
    CwdChanged { cwd: String },
}

#[derive(Debug, Default)]
enum TmuxPassthroughState {
    #[default]
    Ground,
    StartEscape,
    Prefix {
        matched: usize,
    },
    Payload {
        payload: Vec<u8>,
        escaped: bool,
    },
}

pub struct TerminalState {
    parser: Parser,
    grid: TerminalGrid,
    tmux_passthrough_state: TmuxPassthroughState,
}

impl TerminalState {
    pub fn new(width: usize, height: usize) -> Self {
        Self::new_with_passthrough(width, height, true)
    }

    pub fn new_with_passthrough(width: usize, height: usize, allow_passthrough: bool) -> Self {
        Self {
            parser: Parser::new(),
            grid: TerminalGrid::new(width, height, allow_passthrough),
            tmux_passthrough_state: TmuxPassthroughState::default(),
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.grid.resize(width, height);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.grid.allow_passthrough {
            self.parser.advance(&mut self.grid, bytes);
            return;
        }
        let filtered = self.filter_tmux_passthrough(bytes);
        if !filtered.is_empty() {
            self.parser.advance(&mut self.grid, &filtered);
        }
    }

    pub fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.grid.set_allow_passthrough(allow_passthrough);
        if !allow_passthrough {
            self.tmux_passthrough_state = TmuxPassthroughState::Ground;
        }
    }

    pub fn allow_passthrough(&self) -> bool {
        self.grid.allow_passthrough
    }

    pub fn drain_passthrough(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.grid.passthrough_queue)
    }

    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.grid.terminal_events)
    }

    /// Drain any pending response bytes (e.g. cursor position reports).
    pub fn drain_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.grid.response_queue)
    }

    pub fn row_text(&self, row: usize) -> String {
        self.grid.row_text(row)
    }

    pub fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        self.grid.row_cells(row)
    }

    pub fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        self.grid.absolute_row_cells(absolute_row)
    }

    pub fn history_len(&self) -> usize {
        self.grid.history_len()
    }

    pub fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }

    pub fn width(&self) -> usize {
        self.grid.width
    }

    pub fn height(&self) -> usize {
        self.grid.height
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.grid.cursor_x, self.grid.cursor_y)
    }

    pub fn cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        self.grid.cursor_style
    }

    pub fn scrollback_text(&self) -> String {
        self.grid.scrollback_text()
    }

    pub fn history_lines(&self) -> Vec<String> {
        self.grid.history_lines()
    }

    pub fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        self.grid.history_cells()
    }

    pub fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        self.grid.history_tail_lines(max_lines)
    }

    pub fn export_text_hard_lf(&self) -> String {
        self.grid.export_text_hard_lf()
    }

    fn filter_tmux_passthrough(&mut self, bytes: &[u8]) -> Vec<u8> {
        const TMUX_PREFIX: &[u8] = b"tmux;";
        let mut filtered = Vec::with_capacity(bytes.len());

        for &byte in bytes {
            match &mut self.tmux_passthrough_state {
                TmuxPassthroughState::Ground => {
                    if byte == 0x1b {
                        self.tmux_passthrough_state = TmuxPassthroughState::StartEscape;
                    } else {
                        filtered.push(byte);
                    }
                }
                TmuxPassthroughState::StartEscape => {
                    if byte == b'P' {
                        self.tmux_passthrough_state = TmuxPassthroughState::Prefix { matched: 0 };
                    } else {
                        filtered.push(0x1b);
                        filtered.push(byte);
                        self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                    }
                }
                TmuxPassthroughState::Prefix { matched } => {
                    if TMUX_PREFIX
                        .get(*matched)
                        .is_some_and(|expected| *expected == byte)
                    {
                        *matched += 1;
                        if *matched == TMUX_PREFIX.len() {
                            self.tmux_passthrough_state = TmuxPassthroughState::Payload {
                                payload: Vec::new(),
                                escaped: false,
                            };
                        }
                        continue;
                    }

                    filtered.push(0x1b);
                    filtered.push(b'P');
                    filtered.extend_from_slice(&TMUX_PREFIX[..*matched]);
                    filtered.push(byte);
                    self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                }
                TmuxPassthroughState::Payload { payload, escaped } => {
                    if *escaped {
                        match byte {
                            0x1b => payload.push(0x1b),
                            b'\\' => {
                                if !payload.is_empty() {
                                    self.grid.passthrough_queue.push(std::mem::take(payload));
                                }
                                self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                                continue;
                            }
                            _ => {
                                payload.push(0x1b);
                                payload.push(byte);
                            }
                        }
                        *escaped = false;
                        continue;
                    }

                    if byte == 0x1b {
                        *escaped = true;
                    } else {
                        payload.push(byte);
                    }
                }
            }
        }

        filtered
    }
}

struct SavedScreen {
    cells: Vec<StyledCell>,
    scrollback: Vec<HistoryLine>,
    row_boundaries: Vec<RowBoundary>,
    cursor_x: usize,
    cursor_y: usize,
    active_style: CellStyle,
    scroll_top: usize,
    scroll_bottom: usize,
}

struct TerminalGrid {
    width: usize,
    height: usize,
    cells: Vec<StyledCell>,
    scrollback: Vec<HistoryLine>,
    row_boundaries: Vec<RowBoundary>,
    scroll_top: usize,
    scroll_bottom: usize,
    cursor_x: usize,
    cursor_y: usize,
    active_style: CellStyle,
    saved_cursor_x: usize,
    saved_cursor_y: usize,
    saved_style: CellStyle,
    cursor_style: crossterm::cursor::SetCursorStyle,
    saved_screen: Option<SavedScreen>,
    /// Bytes to send back to the child process (e.g. cursor position reports).
    response_queue: Vec<Vec<u8>>,
    /// Insert Replacement Mode (IRM, CSI 4 h/l). When true, printing shifts
    /// existing characters to the right instead of overwriting.
    insert_mode: bool,
    allow_passthrough: bool,
    passthrough_queue: Vec<Vec<u8>>,
    terminal_events: Vec<TerminalEvent>,
}

const MAX_SCROLLBACK_LINES: usize = 10_000;

impl TerminalGrid {
    fn new(width: usize, height: usize, allow_passthrough: bool) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![StyledCell::default(); width * height],
            scrollback: Vec::new(),
            row_boundaries: vec![RowBoundary::None; height],
            scroll_top: 0,
            scroll_bottom: height.saturating_sub(1),
            cursor_x: 0,
            cursor_y: 0,
            active_style: CellStyle::default(),
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            saved_style: CellStyle::default(),
            cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
            saved_screen: None,
            response_queue: Vec::new(),
            insert_mode: false,
            allow_passthrough,
            passthrough_queue: Vec::new(),
            terminal_events: Vec::new(),
        }
    }

    fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.allow_passthrough = allow_passthrough;
        if !allow_passthrough {
            self.passthrough_queue.clear();
        }
    }

    fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return;
        }

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
                self.cells[y * new_width + x] = old_cells[y * old_width + x];
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
        while all_rows.len() > 1 {
            let (cells, _) = all_rows.last().unwrap();
            if !cells.iter().all(|c| *c == StyledCell::default()) {
                break;
            }
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
            new_cells[dst..dst + len].copy_from_slice(&cells[..len]);
            new_boundaries[row_idx] = *boundary;
        }

        // Adjust cursor if it was in a stripped trailing row
        let cursor_abs_row = cursor_abs_row.min(total.saturating_sub(1));
        let cursor_y = cursor_abs_row.saturating_sub(visible_start);

        self.width = new_width;
        self.height = new_height;
        self.cells = new_cells;
        self.row_boundaries = new_boundaries;
        self.scrollback = new_scrollback;
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

            let cell = cells[i];
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
            let cell = line.cells[i];

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
                current_row.push(cell);
                current_row.push(StyledCell {
                    ch: '\0',
                    style: cell.style,
                });
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

    fn row_text(&self, row: usize) -> String {
        if row >= self.height {
            return String::new();
        }
        let start = row * self.width;
        let end = start + self.width;
        self.cells[start..end]
            .iter()
            .filter(|cell| cell.ch != '\0')
            .map(|cell| cell.ch)
            .collect()
    }

    fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        if row >= self.height {
            return Vec::new();
        }
        let start = row * self.width;
        let end = start + self.width;
        self.cells[start..end].to_vec()
    }

    fn trimmed_row_text(&self, row: usize) -> String {
        self.row_text(row).trim_end_matches(' ').to_string()
    }

    fn scrollback_text(&self) -> String {
        self.history_lines().join("\n")
    }

    fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    fn history_lines(&self) -> Vec<String> {
        let mut lines = self
            .scrollback
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        lines.extend((0..self.height).map(|row| self.trimmed_row_text(row)));
        lines
    }

    fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        let mut rows = self
            .scrollback
            .iter()
            .map(|line| self.fit_cells_to_width(&line.cells))
            .collect::<Vec<_>>();
        rows.extend((0..self.height).map(|row| self.row_cells(row)));
        rows
    }

    fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }

        let visible_lines = self.height;
        let total_lines = self.scrollback.len() + visible_lines;
        let keep = total_lines.min(max_lines);
        let scrollback_keep = keep.saturating_sub(visible_lines);
        let visible_start = visible_lines.saturating_sub(keep);

        let mut lines = Vec::with_capacity(keep);
        if scrollback_keep > 0 {
            let start = self.scrollback.len().saturating_sub(scrollback_keep);
            lines.extend(
                self.scrollback[start..]
                    .iter()
                    .map(|line| line.text.clone()),
            );
        }
        lines.extend((visible_start..visible_lines).map(|row| self.trimmed_row_text(row)));
        lines
    }

    fn export_text_hard_lf(&self) -> String {
        let mut out = String::new();
        for line in &self.scrollback {
            out.push_str(&line.text);
            if line.boundary_to_next == RowBoundary::HardLf {
                out.push('\n');
            }
        }
        for row in 0..self.height {
            out.push_str(&self.trimmed_row_text(row));
            if self.row_boundary_to_next(row) == RowBoundary::HardLf {
                out.push('\n');
            }
        }
        out
    }

    fn total_lines(&self) -> usize {
        self.height + self.scrollback.len()
    }

    fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        if self.total_lines() <= absolute_row {
            return vec![StyledCell::default(); self.width];
        }

        let history_len = self.scrollback.len();
        if absolute_row < history_len {
            return self.fit_cells_to_width(&self.scrollback[absolute_row].cells);
        }

        let visible_row = absolute_row - history_len;
        self.row_cells(visible_row)
    }

    fn fit_cells_to_width(&self, cells: &[StyledCell]) -> Vec<StyledCell> {
        Self::fit_cells(cells, self.width)
    }

    fn fit_cells(cells: &[StyledCell], width: usize) -> Vec<StyledCell> {
        if width == 0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(width);
        let mut index = 0usize;
        while index < cells.len() && out.len() < width {
            let cell = cells[index];
            if cell.ch == '\0' {
                out.push(StyledCell::default());
                index += 1;
                continue;
            }

            let cell_width = UnicodeWidthChar::width(cell.ch).unwrap_or(1).max(1);
            if cell_width == 2 {
                if out.len() + 1 >= width {
                    break;
                }
                let Some(continuation) = cells.get(index + 1).copied() else {
                    break;
                };
                if continuation.ch != '\0' {
                    out.push(StyledCell::default());
                    index += 1;
                    continue;
                }
                out.push(cell);
                out.push(continuation);
                index += 2;
                continue;
            }

            out.push(cell);
            index += 1;
        }

        if out.len() < width {
            out.resize(width, StyledCell::default());
        }
        out
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    fn clear_row(&mut self, row: usize) {
        if row >= self.height {
            return;
        }
        let row_start = row * self.width;
        self.cells[row_start..row_start + self.width].fill(StyledCell::default());
        self.row_boundaries[row] = RowBoundary::None;
    }

    fn copy_row(&mut self, dst: usize, src: usize) {
        if dst >= self.height || src >= self.height {
            return;
        }
        let src_start = src * self.width;
        let dst_start = dst * self.width;
        self.cells
            .copy_within(src_start..src_start + self.width, dst_start);
        self.row_boundaries[dst] = self.row_boundary_to_next(src);
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let last_row = self.height.saturating_sub(1);
        let top = top.min(last_row);
        let bottom = bottom.min(last_row);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        }
    }

    fn in_scroll_region(&self, row: usize) -> bool {
        row >= self.scroll_top && row <= self.scroll_bottom
    }

    fn clear_all(&mut self) {
        self.cells.fill(StyledCell::default());
        self.row_boundaries.fill(RowBoundary::None);
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    fn clear_line_from_cursor(&mut self) {
        let row_start = self.cursor_y * self.width;
        for x in self.cursor_x..self.width {
            self.cells[row_start + x] = StyledCell::default();
        }
    }

    fn clear_line_to_cursor(&mut self) {
        let row_start = self.cursor_y * self.width;
        for x in 0..=self.cursor_x.min(self.width.saturating_sub(1)) {
            self.cells[row_start + x] = StyledCell::default();
        }
    }

    fn clear_entire_line(&mut self) {
        self.clear_row(self.cursor_y);
    }

    fn clear_to_end(&mut self) {
        self.clear_line_from_cursor();
        for y in (self.cursor_y + 1)..self.height {
            self.clear_row(y);
        }
    }

    fn clear_to_beginning(&mut self) {
        self.clear_line_to_cursor();
        for y in 0..self.cursor_y {
            self.clear_row(y);
        }
    }

    fn linefeed(&mut self, boundary: RowBoundary) {
        if self.cursor_y < self.row_boundaries.len() {
            self.row_boundaries[self.cursor_y] = boundary;
        }
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up_in_region(self.scroll_top, self.scroll_bottom, 1, true);
        } else {
            self.cursor_y = (self.cursor_y + 1).min(self.height.saturating_sub(1));
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_x = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_x >= self.width {
            // Pending-wrap state: just cancel the wrap, land on last column
            self.cursor_x = self.width.saturating_sub(1);
        } else {
            self.cursor_x = self.cursor_x.saturating_sub(1);
        }
    }

    fn tab(&mut self) {
        let next = ((self.cursor_x / 8) + 1) * 8;
        self.cursor_x = next.min(self.width.saturating_sub(1));
    }

    fn save_cursor(&mut self) {
        self.saved_cursor_x = self.cursor_x.min(self.width.saturating_sub(1));
        self.saved_cursor_y = self.cursor_y;
        self.saved_style = self.active_style;
    }

    fn restore_cursor(&mut self) {
        self.cursor_x = self.saved_cursor_x.min(self.width.saturating_sub(1));
        self.cursor_y = self.saved_cursor_y.min(self.height.saturating_sub(1));
        self.active_style = self.saved_style;
    }

    fn reverse_index(&mut self) {
        if self.cursor_y == self.scroll_top {
            self.scroll_down_in_region(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.cursor_y = self.cursor_y.saturating_sub(1);
        }
    }

    fn put_char(&mut self, ch: char) {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);

        // Wide char doesn't fit at end of line — pad remainder and wrap
        if ch_width == 2 && self.cursor_x + 1 >= self.width {
            if self.cursor_x < self.width {
                let idx = self.idx(self.cursor_x, self.cursor_y);
                self.cells[idx] = StyledCell::default();
            }
            self.cursor_x = 0;
            self.linefeed(RowBoundary::SoftWrap);
        }

        if self.cursor_x >= self.width {
            self.cursor_x = 0;
            self.linefeed(RowBoundary::SoftWrap);
        }

        if self.cursor_y >= self.height {
            self.scroll_up(1);
            self.cursor_y = self.height.saturating_sub(1);
        }

        // IRM: shift cells right before placing the new character
        if self.insert_mode {
            let row_start = self.cursor_y * self.width;
            let end = self.width;
            for x in (self.cursor_x..end).rev() {
                let dst = x + ch_width;
                if dst < end {
                    self.cells[row_start + dst] = self.cells[row_start + x];
                }
                if x < self.cursor_x + ch_width {
                    self.cells[row_start + x] = StyledCell::default();
                }
            }
        }

        // If overwriting a continuation cell, clear the owning wide char
        let idx = self.idx(self.cursor_x, self.cursor_y);
        if self.cells[idx].ch == '\0' && self.cursor_x > 0 {
            let owner_idx = self.idx(self.cursor_x - 1, self.cursor_y);
            self.cells[owner_idx] = StyledCell::default();
        }

        // If overwriting a wide char, clear its continuation cell
        if self.cells[idx].ch != ' '
            && self.cells[idx].ch != '\0'
            && UnicodeWidthChar::width(self.cells[idx].ch).unwrap_or(1) == 2
            && self.cursor_x + 1 < self.width
        {
            let cont_idx = self.idx(self.cursor_x + 1, self.cursor_y);
            self.cells[cont_idx] = StyledCell::default();
        }

        self.cells[idx] = StyledCell {
            ch,
            style: self.active_style,
        };

        // Place continuation cell for wide characters
        if ch_width == 2 && self.cursor_x + 1 < self.width {
            let cont_idx = self.idx(self.cursor_x + 1, self.cursor_y);
            // If the continuation cell overwrites a wide char's continuation, fix owner
            if self.cells[cont_idx].ch == '\0' && self.cursor_x + 1 > 0 {
                // The owner is at cursor_x, which we just wrote — no fixup needed
            }
            // If the continuation cell overwrites a wide char, fix its continuation
            if self.cells[cont_idx].ch != ' '
                && self.cells[cont_idx].ch != '\0'
                && UnicodeWidthChar::width(self.cells[cont_idx].ch).unwrap_or(1) == 2
                && self.cursor_x + 2 < self.width
            {
                let next_cont = self.idx(self.cursor_x + 2, self.cursor_y);
                self.cells[next_cont] = StyledCell::default();
            }
            self.cells[cont_idx] = StyledCell {
                ch: '\0',
                style: self.active_style,
            };
        }

        self.cursor_x += ch_width; // May reach self.width — that's the "pending wrap" state
    }

    fn scroll_up(&mut self, count: usize) {
        if self.height == 0 {
            return;
        }
        self.scroll_up_in_region(0, self.height - 1, count, true);
    }

    fn scroll_up_in_region(
        &mut self,
        top: usize,
        bottom: usize,
        count: usize,
        record_scrollback: bool,
    ) {
        if top > bottom || bottom >= self.height {
            return;
        }

        let region_height = bottom - top + 1;
        let count = count.min(region_height);
        if count == 0 {
            return;
        }

        if record_scrollback && top == 0 {
            for y in top..(top + count) {
                self.push_scrollback_line(
                    self.trimmed_row_text(y),
                    self.row_cells(y),
                    self.row_boundary_to_next(y),
                );
            }
        }

        for offset in 0..(region_height - count) {
            let dst = top + offset;
            let src = dst + count;
            self.copy_row(dst, src);
        }

        for y in (bottom + 1 - count)..=bottom {
            self.clear_row(y);
        }
    }

    fn scroll_down_in_region(&mut self, top: usize, bottom: usize, count: usize) {
        if top > bottom || bottom >= self.height {
            return;
        }

        let region_height = bottom - top + 1;
        let count = count.min(region_height);
        if count == 0 {
            return;
        }

        for offset in (0..(region_height - count)).rev() {
            let src = top + offset;
            let dst = src + count;
            self.copy_row(dst, src);
        }

        for y in top..(top + count) {
            self.clear_row(y);
        }
    }

    fn insert_lines_at_cursor(&mut self, count: usize) {
        if !self.in_scroll_region(self.cursor_y) {
            return;
        }
        self.scroll_down_in_region(self.cursor_y, self.scroll_bottom, count);
    }

    fn delete_lines_at_cursor(&mut self, count: usize) {
        if !self.in_scroll_region(self.cursor_y) {
            return;
        }
        self.scroll_up_in_region(self.cursor_y, self.scroll_bottom, count, false);
    }

    fn scroll_up_current_region(&mut self, count: usize) {
        self.scroll_up_in_region(self.scroll_top, self.scroll_bottom, count, true);
    }

    fn scroll_down_current_region(&mut self, count: usize) {
        self.scroll_down_in_region(self.scroll_top, self.scroll_bottom, count);
    }

    fn row_boundary_to_next(&self, row: usize) -> RowBoundary {
        self.row_boundaries
            .get(row)
            .copied()
            .unwrap_or(RowBoundary::None)
    }

    fn push_scrollback_line(
        &mut self,
        text: String,
        cells: Vec<StyledCell>,
        boundary_to_next: RowBoundary,
    ) {
        self.scrollback.push(HistoryLine {
            text,
            cells,
            boundary_to_next,
        });
        let overflow = self.scrollback.len().saturating_sub(MAX_SCROLLBACK_LINES);
        if overflow > 0 {
            self.scrollback.drain(0..overflow);
        }
    }

    fn enter_alternate_screen(&mut self) {
        if self.saved_screen.is_some() {
            return;
        }
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

    fn leave_alternate_screen(&mut self) {
        let Some(saved) = self.saved_screen.take() else {
            return;
        };
        self.cells = saved.cells;
        self.scrollback = saved.scrollback;
        self.row_boundaries = saved.row_boundaries;
        self.cursor_x = saved.cursor_x;
        self.cursor_y = saved.cursor_y;
        self.active_style = saved.active_style;
        self.scroll_top = saved.scroll_top;
        self.scroll_bottom = saved.scroll_bottom;
    }

    fn csi_param(params: &Params, index: usize, default: usize) -> usize {
        params
            .iter()
            .nth(index)
            .and_then(|values| values.first().copied())
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    fn to_u8(value: Option<u16>) -> Option<u8> {
        value.and_then(|v| u8::try_from(v).ok())
    }

    fn sgr_values(params: &Params) -> Vec<Option<u16>> {
        let mut values = Vec::new();
        for param in params.iter() {
            if param.is_empty() {
                values.push(None);
            } else {
                values.extend(param.iter().copied().map(Some));
            }
        }
        if values.is_empty() {
            values.push(Some(0));
        }
        values
    }

    fn parse_extended_color(values: &[Option<u16>], start: usize) -> (Option<Color>, usize) {
        let Some(mode) = values.get(start).copied().flatten() else {
            return (None, 0);
        };

        match mode {
            5 => {
                let consumed = values.len().saturating_sub(start).min(2);
                let color =
                    Self::to_u8(values.get(start + 1).copied().flatten()).map(Color::AnsiValue);
                (color, consumed.max(1))
            }
            2 => {
                let consumed = values.len().saturating_sub(start).min(4);
                let color = match (
                    Self::to_u8(values.get(start + 1).copied().flatten()),
                    Self::to_u8(values.get(start + 2).copied().flatten()),
                    Self::to_u8(values.get(start + 3).copied().flatten()),
                ) {
                    (Some(r), Some(g), Some(b)) => Some(Color::Rgb { r, g, b }),
                    _ => None,
                };
                (color, consumed.max(1))
            }
            _ => (None, 1),
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        let values = Self::sgr_values(params);
        let mut i = 0;
        while i < values.len() {
            let code = values[i].unwrap_or(0);
            match code {
                0 => {
                    self.active_style = CellStyle::default();
                }
                1 => self.active_style.bold = true,
                2 => self.active_style.dim = true,
                3 => self.active_style.italic = true,
                4 => self.active_style.underlined = true,
                5 => self.active_style.slow_blink = true,
                6 => self.active_style.rapid_blink = true,
                7 => self.active_style.reverse = true,
                8 => self.active_style.hidden = true,
                9 => self.active_style.crossed_out = true,
                21 => self.active_style.bold = false,
                22 => {
                    self.active_style.bold = false;
                    self.active_style.dim = false;
                }
                23 => self.active_style.italic = false,
                24 => self.active_style.underlined = false,
                25 => {
                    self.active_style.slow_blink = false;
                    self.active_style.rapid_blink = false;
                }
                27 => self.active_style.reverse = false,
                28 => self.active_style.hidden = false,
                29 => self.active_style.crossed_out = false,
                30..=37 => self.active_style.fg = Some(Color::AnsiValue((code - 30) as u8)),
                39 => self.active_style.fg = None,
                40..=47 => self.active_style.bg = Some(Color::AnsiValue((code - 40) as u8)),
                49 => self.active_style.bg = None,
                90..=97 => {
                    self.active_style.fg = Some(Color::AnsiValue((code - 90 + 8) as u8));
                }
                100..=107 => {
                    self.active_style.bg = Some(Color::AnsiValue((code - 100 + 8) as u8));
                }
                38 | 48 => {
                    let (color, consumed) = Self::parse_extended_color(&values, i + 1);
                    if let Some(color) = color {
                        if code == 38 {
                            self.active_style.fg = Some(color);
                        } else {
                            self.active_style.bg = Some(color);
                        }
                    }
                    i += consumed + 1;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn osc_payload_bytes(params: &[&[u8]], start: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        for (index, param) in params.iter().enumerate().skip(start) {
            if index > start {
                payload.push(b';');
            }
            payload.extend_from_slice(param);
        }
        payload
    }

    fn encode_osc_sequence(params: &[&[u8]], bell_terminated: bool) -> Vec<u8> {
        let mut sequence = Vec::with_capacity(8);
        sequence.extend_from_slice(b"\x1b]");
        sequence.extend(Self::osc_payload_bytes(params, 0));
        if bell_terminated {
            sequence.push(0x07);
        } else {
            sequence.extend_from_slice(b"\x1b\\");
        }
        sequence
    }
}

fn parse_osc7_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("file://") {
        let path_start = rest.find('/')?;
        let path = rest[path_start..].trim_matches('\0');
        let decoded = percent_decode(path.as_bytes())?;
        return sanitize_display_text(&decoded);
    }

    sanitize_display_text(trimmed)
}

fn percent_decode(bytes: &[u8]) -> Option<String> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = bytes[index + 1] as char;
            let low = bytes[index + 2] as char;
            let hex = [high, low].iter().collect::<String>();
            let value = u8::from_str_radix(&hex, 16).ok()?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn sanitize_display_text(input: &str) -> Option<String> {
    const MAX_BYTES: usize = 256;

    let mut clean = String::new();
    for ch in input.chars() {
        if ch.is_control() {
            continue;
        }
        let ch_len = ch.len_utf8();
        if clean.len() + ch_len > MAX_BYTES {
            break;
        }
        clean.push(ch);
    }

    (!clean.is_empty()).then_some(clean)
}

impl Perform for TerminalGrid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.linefeed(RowBoundary::HardLf);
            }
            b'\r' => self.carriage_return(),
            0x08 => self.backspace(),
            b'\t' => self.tab(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(RowBoundary::None),
            b'M' => self.reverse_index(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(ps) = params.first() else {
            return;
        };

        match *ps {
            b"8" => {
                if self.allow_passthrough {
                    self.passthrough_queue
                        .push(Self::encode_osc_sequence(params, bell_terminated));
                }
            }
            b"0" | b"2" => {
                let payload = Self::osc_payload_bytes(params, 1);
                let title = if payload.is_empty() {
                    None
                } else {
                    let Ok(raw) = String::from_utf8(payload) else {
                        return;
                    };
                    sanitize_display_text(&raw)
                };
                self.terminal_events
                    .push(TerminalEvent::TitleChanged { title });
            }
            b"7" => {
                let payload = Self::osc_payload_bytes(params, 1);
                if payload.is_empty() {
                    return;
                }
                let Ok(raw) = String::from_utf8(payload) else {
                    return;
                };
                if let Some(cwd) = parse_osc7_path(&raw) {
                    self.terminal_events.push(TerminalEvent::CwdChanged { cwd });
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Clamp cursor_x (clear pending-wrap) only for cursor-movement sequences
        match action {
            'A' | 'B' | 'C' | 'D' | 'H' | 'f' | 'G' | 'd' | 'E' | 'F' | 's' | 'u' | 'J' | 'K'
            | 'X' | 'P' | '@' | 'L' | 'M' => {
                self.cursor_x = self.cursor_x.min(self.width.saturating_sub(1));
            }
            _ => {}
        }

        match action {
            'A' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(delta);
            }
            'B' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = (self.cursor_y + delta).min(self.height.saturating_sub(1));
            }
            'C' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_x = (self.cursor_x + delta).min(self.width.saturating_sub(1));
            }
            'D' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_x = self.cursor_x.saturating_sub(delta);
            }
            'H' | 'f' => {
                let row = Self::csi_param(params, 0, 1);
                let col = Self::csi_param(params, 1, 1);
                self.cursor_y = row.saturating_sub(1).min(self.height.saturating_sub(1));
                self.cursor_x = col.saturating_sub(1).min(self.width.saturating_sub(1));
            }
            'J' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    1 => self.clear_to_beginning(),
                    2 => self.clear_all(),
                    3 => self.clear_scrollback(),
                    _ => self.clear_to_end(),
                }
            }
            'K' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    1 => self.clear_line_to_cursor(),
                    2 => self.clear_entire_line(),
                    _ => self.clear_line_from_cursor(),
                }
            }
            'L' => {
                let count = Self::csi_param(params, 0, 1);
                self.insert_lines_at_cursor(count);
            }
            'M' => {
                let count = Self::csi_param(params, 0, 1);
                self.delete_lines_at_cursor(count);
            }
            'S' => {
                let count = Self::csi_param(params, 0, 1);
                self.scroll_up_current_region(count);
            }
            'T' => {
                let count = Self::csi_param(params, 0, 1);
                self.scroll_down_current_region(count);
            }
            'r' => {
                let top = Self::csi_param(params, 0, 1);
                let bottom = Self::csi_param(params, 1, self.height);
                self.set_scroll_region(top.saturating_sub(1), bottom.saturating_sub(1));
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            'G' => {
                // CHA — Cursor Horizontal Absolute
                let col = Self::csi_param(params, 0, 1);
                self.cursor_x = col.saturating_sub(1).min(self.width.saturating_sub(1));
            }
            'd' => {
                // VPA — Vertical Position Absolute
                let row = Self::csi_param(params, 0, 1);
                self.cursor_y = row.saturating_sub(1).min(self.height.saturating_sub(1));
            }
            'E' => {
                // CNL — Cursor Next Line
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = (self.cursor_y + delta).min(self.height.saturating_sub(1));
                self.cursor_x = 0;
            }
            'F' => {
                // CPL — Cursor Previous Line
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(delta);
                self.cursor_x = 0;
            }
            'X' => {
                // ECH — Erase Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                for i in 0..count {
                    let x = self.cursor_x + i;
                    if x >= self.width {
                        break;
                    }
                    self.cells[row_start + x] = StyledCell::default();
                }
            }
            'P' => {
                // DCH — Delete Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                let end = self.width;
                for x in self.cursor_x..end {
                    let src = x + count;
                    self.cells[row_start + x] = if src < end {
                        self.cells[row_start + src]
                    } else {
                        StyledCell::default()
                    };
                }
            }
            '@' => {
                // ICH — Insert Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                let end = self.width;
                for x in (self.cursor_x..end).rev() {
                    let dst = x + count;
                    if dst < end {
                        self.cells[row_start + dst] = self.cells[row_start + x];
                    }
                    if x < self.cursor_x + count {
                        self.cells[row_start + x] = StyledCell::default();
                    }
                }
            }
            'h' if intermediates == [b'?'] => {
                for param in params.iter() {
                    if matches!(param[0], 47 | 1047 | 1049) {
                        self.enter_alternate_screen();
                    }
                }
            }
            'l' if intermediates == [b'?'] => {
                for param in params.iter() {
                    if matches!(param[0], 47 | 1047 | 1049) {
                        self.leave_alternate_screen();
                    }
                }
            }
            // SM — Set Mode (ANSI modes, no `?` prefix)
            'h' if intermediates.is_empty() => {
                for param in params.iter() {
                    if param[0] == 4 {
                        self.insert_mode = true;
                    }
                }
            }
            // RM — Reset Mode (ANSI modes, no `?` prefix)
            'l' if intermediates.is_empty() => {
                for param in params.iter() {
                    if param[0] == 4 {
                        self.insert_mode = false;
                    }
                }
            }
            'n' if intermediates.is_empty() => {
                // DSR — Device Status Report
                let ps = Self::csi_param(params, 0, 0);
                match ps {
                    5 => {
                        // Status report: OK
                        self.response_queue.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // Cursor position report (1-based)
                        let row = self.cursor_y + 1;
                        let col = self.cursor_x.min(self.width.saturating_sub(1)) + 1;
                        self.response_queue
                            .push(format!("\x1b[{row};{col}R").into_bytes());
                    }
                    _ => {}
                }
            }
            'n' if intermediates == [b'?'] => {
                // DECDSR — DEC private Device Status Report
                let ps = Self::csi_param(params, 0, 0);
                if ps == 6 {
                    // DEC cursor position report (1-based)
                    let row = self.cursor_y + 1;
                    let col = self.cursor_x.min(self.width.saturating_sub(1)) + 1;
                    self.response_queue
                        .push(format!("\x1b[?{row};{col}R").into_bytes());
                }
            }
            't' if intermediates.is_empty() => {
                // XTWINOPS — Window manipulation
                let ps = Self::csi_param(params, 0, 0);
                if ps == 18 {
                    // Report text area size in characters
                    self.response_queue
                        .push(format!("\x1b[8;{};{}t", self.height, self.width).into_bytes());
                }
            }
            'c' if intermediates.is_empty() || intermediates == [b'>'] => {
                // DA — Device Attributes
                // Respond as a VT220-compatible terminal
                if intermediates.is_empty() {
                    self.response_queue.push(b"\x1b[?62;22c".to_vec());
                } else {
                    // Secondary DA
                    self.response_queue.push(b"\x1b[>1;1;0c".to_vec());
                }
            }
            's' if intermediates.is_empty() => {
                self.save_cursor();
            }
            'u' if intermediates.is_empty() => {
                self.restore_cursor();
            }
            'q' if intermediates == [b' '] => {
                // DECSCUSR — Set Cursor Style
                let ps = Self::csi_param(params, 0, 0);
                self.cursor_style = match ps {
                    0 | 1 => crossterm::cursor::SetCursorStyle::BlinkingBlock,
                    2 => crossterm::cursor::SetCursorStyle::SteadyBlock,
                    3 => crossterm::cursor::SetCursorStyle::BlinkingUnderScore,
                    4 => crossterm::cursor::SetCursorStyle::SteadyUnderScore,
                    5 => crossterm::cursor::SetCursorStyle::BlinkingBar,
                    6 => crossterm::cursor::SetCursorStyle::SteadyBar,
                    _ => crossterm::cursor::SetCursorStyle::DefaultUserShape,
                };
            }
            'm' => {
                self.apply_sgr(params);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::{CellStyle, StyledCell, TerminalEvent, TerminalState};

    #[test]
    fn writes_and_wraps() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"abcde");
        assert_eq!(state.row_text(0), "abcd");
        assert_eq!(state.row_text(1), "e   ");
    }

    #[test]
    fn handles_cursor_and_clear() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"hello!");
        state.feed(b"\x1b[1;1H");
        state.feed(b"X");
        state.feed(b"\x1b[2J");
        assert_eq!(state.row_text(0), "      ");
        assert_eq!(state.row_text(1), "      ");
    }

    #[test]
    fn lf_moves_down_without_cr() {
        let mut state = TerminalState::new(6, 3);
        state.feed(b"ab\nX");
        // \n only moves down, cursor stays at column 2
        assert_eq!(state.row_text(0), "ab    ");
        assert_eq!(state.row_text(1), "  X   ");
    }

    #[test]
    fn cr_lf_moves_to_start_of_next_line() {
        let mut state = TerminalState::new(6, 3);
        state.feed(b"ab\r\nX");
        assert_eq!(state.row_text(0), "ab    ");
        assert_eq!(state.row_text(1), "X     ");
    }

    #[test]
    fn erase_line_from_cursor_mode_zero() {
        let mut state = TerminalState::new(16, 1);
        state.feed(b"ABCDEFGH");
        state.feed(b"\x1b[1;4H\x1b[K");
        assert_eq!(state.row_text(0), "ABC             ");
    }

    #[test]
    fn erase_line_modes_one_and_two() {
        let mut state = TerminalState::new(16, 1);
        state.feed(b"ABCDEFGH");
        state.feed(b"\x1b[1;4H\x1b[1K");
        assert_eq!(state.row_text(0), "    EFGH        ");

        state.feed(b"\x1b[1;1HABCDEFGH");
        state.feed(b"\x1b[1;5H\x1b[2K");
        assert_eq!(state.row_text(0), "                ");
    }

    #[test]
    fn applies_sgr_styles_and_resets() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"\x1b[31;44;1;3;4;5;6;7;8;9mA\x1b[0mB");

        let row = state.row_cells(0);
        assert_eq!(row[0].ch, 'A');
        assert_eq!(row[0].style.fg, Some(Color::AnsiValue(1)));
        assert_eq!(row[0].style.bg, Some(Color::AnsiValue(4)));
        assert!(row[0].style.bold);
        assert!(row[0].style.italic);
        assert!(row[0].style.underlined);
        assert!(row[0].style.slow_blink);
        assert!(row[0].style.rapid_blink);
        assert!(row[0].style.reverse);
        assert!(row[0].style.hidden);
        assert!(row[0].style.crossed_out);

        assert_eq!(
            row[1],
            StyledCell {
                ch: 'B',
                style: CellStyle::default()
            }
        );
    }

    #[test]
    fn parses_256_and_rgb_colors() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"\x1b[38;5;196;48;2;12;34;56mX");

        let row = state.row_cells(0);
        assert_eq!(row[0].style.fg, Some(Color::AnsiValue(196)));
        assert_eq!(
            row[0].style.bg,
            Some(Color::Rgb {
                r: 12,
                g: 34,
                b: 56
            })
        );
    }

    #[test]
    fn malformed_extended_colors_are_ignored_safely() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"\x1b[38;2;255;0mA\x1b[48;5mB");

        let row = state.row_cells(0);
        assert_eq!(
            row[0],
            StyledCell {
                ch: 'A',
                style: CellStyle::default()
            }
        );
        assert_eq!(
            row[1],
            StyledCell {
                ch: 'B',
                style: CellStyle::default()
            }
        );
    }

    #[test]
    fn supports_attribute_reset_codes() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"\x1b[1;2;3;4;5;6;7;8;9m");
        state.feed(b"\x1b[22;23;24;25;27;28;29mA");

        let row = state.row_cells(0);
        assert_eq!(
            row[0],
            StyledCell {
                ch: 'A',
                style: CellStyle {
                    fg: None,
                    bg: None,
                    bold: false,
                    dim: false,
                    italic: false,
                    underlined: false,
                    slow_blink: false,
                    rapid_blink: false,
                    reverse: false,
                    hidden: false,
                    crossed_out: false,
                },
            }
        );
    }

    #[test]
    fn resize_preserves_existing_cells_and_cursor() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"abcd");
        state.feed(b"\x1b[2;1H"); // explicitly move to row 2, col 1
        state.feed(b"Z");
        state.resize(6, 3);

        assert_eq!(state.row_text(0), "abcd  ");
        assert_eq!(state.row_text(1), "Z     ");
        assert_eq!(state.cursor(), (1, 1));
    }

    #[test]
    fn resize_shrink_reflows_content_to_last_visible_row() {
        let mut state = TerminalState::new(6, 3);
        state.feed(b"hello\r\nworld");
        state.resize(3, 1);

        // With reflow: "hello"→"hel"(SW)+"lo "(HardLf), "world"→"wor"(SW)+"ld "(None)
        // Height 1: only last row visible
        assert_eq!(state.row_text(0), "ld ");
        assert_eq!(state.cursor(), (2, 0));
    }

    #[test]
    fn scrollback_tracks_lines_scrolled_off_screen() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"line1\r\nline2\r\nline3");

        let scrollback = state.scrollback_text();
        assert!(scrollback.contains("line1"));
        assert!(scrollback.contains("line2"));
        assert!(scrollback.contains("line3"));
    }

    #[test]
    fn absolute_row_cells_preserves_style_for_scrollback_rows() {
        let mut state = TerminalState::new(6, 1);
        state.feed(b"\x1b[31mA\x1b[0m\r\nB");

        let row = state.absolute_row_cells(0);
        assert_eq!(row[0].ch, 'A');
        assert_eq!(row[0].style.fg, Some(Color::AnsiValue(1)));
        assert_eq!(row[0].style.bg, None);
    }

    #[test]
    fn history_tail_lines_returns_recent_lines() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"line1\r\nline2\r\nline3");

        assert_eq!(
            state.history_tail_lines(2),
            vec!["line2".to_string(), "line3".to_string()]
        );
        assert_eq!(
            state.history_tail_lines(3),
            vec![
                "line1".to_string(),
                "line2".to_string(),
                "line3".to_string()
            ]
        );
    }

    #[test]
    fn soft_wrap_does_not_emit_newline_in_export() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"abcde");
        assert_eq!(state.export_text_hard_lf(), "abcde");
    }

    #[test]
    fn hard_lf_emits_newline_in_export() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"abc\r\nxyz");
        assert_eq!(state.export_text_hard_lf(), "abc\nxyz");
    }

    #[test]
    fn mixed_soft_wrap_and_hard_lf_export() {
        let mut state = TerminalState::new(4, 3);
        state.feed(b"abcde\r\nfg");
        assert_eq!(state.export_text_hard_lf(), "abcde\nfg");
    }

    #[test]
    fn trailing_hard_lf_preserved() {
        let mut with_lf = TerminalState::new(8, 2);
        with_lf.feed(b"abc\r\n");
        assert_eq!(with_lf.export_text_hard_lf(), "abc\n");

        let mut without_lf = TerminalState::new(8, 2);
        without_lf.feed(b"abc");
        assert_eq!(without_lf.export_text_hard_lf(), "abc");
    }

    #[test]
    fn scrolloff_preserves_boundary_types() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"abcde\r\nfg\r\nh");
        assert_eq!(state.export_text_hard_lf(), "abcde\nfg\nh");
    }

    #[test]
    fn csi_scroll_sequences_shift_full_viewport() {
        let mut state = TerminalState::new(3, 4);
        state.feed(b"A\r\nB\r\nC\r\nD");

        state.feed(b"\x1b[1S");
        state.feed(b"\x1b[4;1HE");
        assert_eq!(state.row_text(0), "B  ");
        assert_eq!(state.row_text(1), "C  ");
        assert_eq!(state.row_text(2), "D  ");
        assert_eq!(state.row_text(3), "E  ");

        state.feed(b"\x1b[1T");
        state.feed(b"\x1b[1;1HA");
        assert_eq!(state.row_text(0), "A  ");
        assert_eq!(state.row_text(1), "B  ");
        assert_eq!(state.row_text(2), "C  ");
        assert_eq!(state.row_text(3), "D  ");
    }

    #[test]
    fn csi_scroll_region_only_moves_rows_inside_margins() {
        let mut state = TerminalState::new(3, 5);
        state.feed(b"A\r\nB\r\nC\r\nD\r\nE");

        state.feed(b"\x1b[1;4r");
        state.feed(b"\x1b[1S");
        state.feed(b"\x1b[4;1HX");

        assert_eq!(state.row_text(0), "B  ");
        assert_eq!(state.row_text(1), "C  ");
        assert_eq!(state.row_text(2), "D  ");
        assert_eq!(state.row_text(3), "X  ");
        assert_eq!(state.row_text(4), "E  ");
    }

    #[test]
    fn csi_insert_and_delete_lines_shift_region() {
        let mut state = TerminalState::new(3, 4);
        state.feed(b"A\r\nB\r\nC\r\nD");

        state.feed(b"\x1b[2;1H\x1b[L");
        state.feed(b"\x1b[2;1HX");
        assert_eq!(state.row_text(0), "A  ");
        assert_eq!(state.row_text(1), "X  ");
        assert_eq!(state.row_text(2), "B  ");
        assert_eq!(state.row_text(3), "C  ");

        state.feed(b"\x1b[2;1H\x1b[M");
        assert_eq!(state.row_text(0), "A  ");
        assert_eq!(state.row_text(1), "B  ");
        assert_eq!(state.row_text(2), "C  ");
        assert_eq!(state.row_text(3), "   ");
    }

    #[test]
    fn csi_g_moves_cursor_to_column() {
        let mut state = TerminalState::new(10, 1);
        state.feed(b"ABCDEFGHIJ");
        // CSI 4 G → move cursor to column 4 (1-based), then overwrite
        state.feed(b"\x1b[4GX");
        assert_eq!(state.row_text(0), "ABCXEFGHIJ");
    }

    #[test]
    fn csi_g_defaults_to_column_one() {
        let mut state = TerminalState::new(10, 1);
        state.feed(b"ABCDE");
        // CSI G (no param) → move cursor to column 1
        state.feed(b"\x1b[GX");
        assert_eq!(state.row_text(0), "XBCDE     ");
    }

    #[test]
    fn csi_d_moves_cursor_to_row() {
        let mut state = TerminalState::new(5, 4);
        // CSI 3 d → move to row 3 (1-based), then write
        state.feed(b"\x1b[3dX");
        assert_eq!(state.row_text(0), "     ");
        assert_eq!(state.row_text(1), "     ");
        assert_eq!(state.row_text(2), "X    ");
    }

    #[test]
    fn csi_e_moves_to_beginning_of_next_line() {
        let mut state = TerminalState::new(5, 4);
        state.feed(b"AB");
        // CSI 2 E → move to beginning of line 2 lines down
        state.feed(b"\x1b[2EX");
        assert_eq!(state.row_text(0), "AB   ");
        assert_eq!(state.row_text(1), "     ");
        assert_eq!(state.row_text(2), "X    ");
        assert_eq!(state.cursor(), (1, 2));
    }

    #[test]
    fn csi_f_moves_to_beginning_of_previous_line() {
        let mut state = TerminalState::new(5, 4);
        state.feed(b"\x1b[4;3H"); // move to row 4, col 3
        // CSI 2 F → move to beginning of line 2 lines up
        state.feed(b"\x1b[2FX");
        assert_eq!(state.row_text(1), "X    ");
        assert_eq!(state.cursor(), (1, 1));
    }

    #[test]
    fn csi_x_erases_characters() {
        let mut state = TerminalState::new(10, 1);
        state.feed(b"ABCDEFGHIJ");
        // Move to column 3 (1-based) and erase 4 chars
        state.feed(b"\x1b[3G\x1b[4X");
        assert_eq!(state.row_text(0), "AB    GHIJ");
    }

    #[test]
    fn csi_p_deletes_characters_shifting_left() {
        let mut state = TerminalState::new(8, 1);
        state.feed(b"ABCDEFGH");
        // Move to column 3 (1-based), delete 2 chars
        state.feed(b"\x1b[3G\x1b[2P");
        assert_eq!(state.row_text(0), "ABEFGH  ");
    }

    #[test]
    fn csi_at_inserts_blank_characters_shifting_right() {
        let mut state = TerminalState::new(8, 1);
        state.feed(b"ABCDEFGH");
        // Move to column 3 (1-based), insert 2 blanks
        state.feed(b"\x1b[3G\x1b[2@");
        assert_eq!(state.row_text(0), "AB  CDEF");
    }

    #[test]
    fn deferred_wrap_does_not_wrap_immediately() {
        let mut state = TerminalState::new(4, 2);
        // Write exactly 4 chars in a 4-wide grid
        state.feed(b"abcd");
        // Cursor should be at column 4 (pending wrap), still on row 0
        // A \r should bring us back to column 0 of the same row
        state.feed(b"\rX");
        assert_eq!(state.row_text(0), "Xbcd");
        assert_eq!(state.row_text(1), "    ");
    }

    #[test]
    fn deferred_wrap_triggers_on_next_char() {
        let mut state = TerminalState::new(4, 2);
        // Write exactly 4 chars, then one more triggers wrap
        state.feed(b"abcde");
        assert_eq!(state.row_text(0), "abcd");
        assert_eq!(state.row_text(1), "e   ");
        assert_eq!(state.cursor(), (1, 1));
    }

    #[test]
    fn deferred_wrap_with_cr_lf() {
        // Programs that write exactly `width` chars followed by \r\n
        // should not produce double newlines
        let mut state = TerminalState::new(4, 3);
        state.feed(b"abcd\r\nef");
        assert_eq!(state.row_text(0), "abcd");
        assert_eq!(state.row_text(1), "ef  ");
        assert_eq!(state.row_text(2), "    ");
    }

    #[test]
    fn alternate_screen_restores_content_on_leave() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"hello!");
        assert_eq!(state.row_text(0), "hello!");

        // Enter alternate screen (DECSET 1049)
        state.feed(b"\x1b[?1049h");
        assert_eq!(state.row_text(0), "      ");

        // Draw something on alt screen
        state.feed(b"TIG UI");
        assert_eq!(state.row_text(0), "TIG UI");

        // Leave alternate screen (DECRST 1049)
        state.feed(b"\x1b[?1049l");
        assert_eq!(state.row_text(0), "hello!");
    }

    #[test]
    fn alternate_screen_redundant_enter_is_noop() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"first!");

        state.feed(b"\x1b[?1049h");
        state.feed(b"alt1");

        // Second enter should not overwrite saved screen
        state.feed(b"\x1b[?1049h");
        state.feed(b"alt2");

        state.feed(b"\x1b[?1049l");
        assert_eq!(state.row_text(0), "first!");
    }

    #[test]
    fn alternate_screen_leave_without_enter_is_noop() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"hello!");

        state.feed(b"\x1b[?1049l");
        assert_eq!(state.row_text(0), "hello!");
    }

    #[test]
    fn alternate_screen_mode_47_works() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"ABCD");
        state.feed(b"\x1b[?47h");
        assert_eq!(state.row_text(0), "    ");
        state.feed(b"XY");
        state.feed(b"\x1b[?47l");
        assert_eq!(state.row_text(0), "ABCD");
    }

    #[test]
    fn alternate_screen_mode_1047_works() {
        let mut state = TerminalState::new(4, 1);
        state.feed(b"ABCD");
        state.feed(b"\x1b[?1047h");
        assert_eq!(state.row_text(0), "    ");
        state.feed(b"XY");
        state.feed(b"\x1b[?1047l");
        assert_eq!(state.row_text(0), "ABCD");
    }

    #[test]
    fn esc_7_8_save_restore_cursor() {
        let mut state = TerminalState::new(10, 4);
        state.feed(b"\x1b[3;5H"); // move to row 3, col 5
        state.feed(b"\x1b7"); // ESC 7 — save cursor
        state.feed(b"\x1b[1;1H"); // move to row 1, col 1
        state.feed(b"X");
        state.feed(b"\x1b8"); // ESC 8 — restore cursor
        state.feed(b"Y");
        assert_eq!(state.row_text(0), "X         ");
        assert_eq!(state.row_text(2), "    Y     ");
        assert_eq!(state.cursor(), (5, 2));
    }

    #[test]
    fn esc_d_index_scrolls_at_bottom() {
        let mut state = TerminalState::new(3, 3);
        state.feed(b"A\r\nB\r\nC");
        // Cursor is at row 2 (bottom). ESC D should scroll up.
        state.feed(b"\x1bD");
        state.feed(b"\x1b[3;1HX");
        assert_eq!(state.row_text(0), "B  ");
        assert_eq!(state.row_text(1), "C  ");
        assert_eq!(state.row_text(2), "X  ");
    }

    #[test]
    fn esc_m_reverse_index_scrolls_at_top() {
        let mut state = TerminalState::new(3, 3);
        state.feed(b"A\r\nB\r\nC");
        state.feed(b"\x1b[1;1H"); // move to top
        // ESC M at top should scroll down
        state.feed(b"\x1bM");
        state.feed(b"\x1b[1;1HX");
        assert_eq!(state.row_text(0), "X  ");
        assert_eq!(state.row_text(1), "A  ");
        assert_eq!(state.row_text(2), "B  ");
    }

    #[test]
    fn csi_s_u_save_restore_cursor() {
        let mut state = TerminalState::new(10, 4);
        state.feed(b"\x1b[2;6H"); // row 2, col 6
        state.feed(b"\x1b[s"); // CSI s — save cursor
        state.feed(b"\x1b[1;1HX");
        state.feed(b"\x1b[u"); // CSI u — restore cursor
        state.feed(b"Y");
        assert_eq!(state.row_text(0), "X         ");
        assert_eq!(state.row_text(1), "     Y    ");
    }

    #[test]
    fn csi_j_mode_1_clears_to_beginning() {
        let mut state = TerminalState::new(6, 3);
        state.feed(b"AAAAAA");
        state.feed(b"\x1b[2;1HBBBBBB");
        state.feed(b"\x1b[3;1HCCCCCC");
        // Move to row 2, col 4 and clear to beginning
        state.feed(b"\x1b[2;4H\x1b[1J");
        assert_eq!(state.row_text(0), "      "); // fully cleared
        assert_eq!(state.row_text(1), "    BB"); // cleared up to cursor
        assert_eq!(state.row_text(2), "CCCCCC"); // untouched
    }

    #[test]
    fn csi_j_mode_3_clears_scrollback_only() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"1111\r\n2222\r\n3333");
        assert!(
            state.history_len() > 0,
            "expected scrollback before CSI 3 J"
        );

        state.feed(b"\x1b[3J");

        assert_eq!(state.history_len(), 0);
        assert_eq!(state.row_text(0), "2222");
        assert_eq!(state.row_text(1), "3333");
    }

    #[test]
    fn pending_wrap_preserved_across_sgr() {
        let mut state = TerminalState::new(4, 2);
        // Write exactly 4 chars (pending wrap)
        state.feed(b"abcd");
        // SGR color change should NOT clear pending wrap
        state.feed(b"\x1b[31m");
        // Next char should trigger wrap to next line
        state.feed(b"X");
        assert_eq!(state.row_text(0), "abcd");
        assert_eq!(state.row_text(1), "X   ");
    }

    #[test]
    fn pending_wrap_cleared_by_cursor_movement() {
        let mut state = TerminalState::new(4, 2);
        state.feed(b"abcd");
        // CUP should clear pending wrap
        state.feed(b"\x1b[1;4H");
        state.feed(b"X");
        assert_eq!(state.row_text(0), "abcX");
        assert_eq!(state.row_text(1), "    ");
    }

    #[test]
    fn cursor_style_set_via_decscusr() {
        let mut state = TerminalState::new(10, 2);
        assert_eq!(
            state.cursor_style(),
            crossterm::cursor::SetCursorStyle::DefaultUserShape
        );

        // CSI 5 SP q → blinking bar
        state.feed(b"\x1b[5 q");
        assert_eq!(
            state.cursor_style(),
            crossterm::cursor::SetCursorStyle::BlinkingBar
        );

        // CSI 2 SP q → steady block
        state.feed(b"\x1b[2 q");
        assert_eq!(
            state.cursor_style(),
            crossterm::cursor::SetCursorStyle::SteadyBlock
        );

        // CSI 0 SP q → blinking block (default)
        state.feed(b"\x1b[0 q");
        assert_eq!(
            state.cursor_style(),
            crossterm::cursor::SetCursorStyle::BlinkingBlock
        );
    }

    #[test]
    fn backspace_from_pending_wrap_lands_on_last_column() {
        let mut state = TerminalState::new(4, 2);
        // Write 4 chars → cursor at column 4 (pending wrap)
        state.feed(b"abcd");
        assert_eq!(state.cursor(), (4, 0));
        // BS should land on last column (3), not column 2
        state.feed(b"\x08X");
        assert_eq!(state.row_text(0), "abcX");
        assert_eq!(state.row_text(1), "    ");
    }

    #[test]
    fn cursor_position_report_returns_1_based_position() {
        let mut state = TerminalState::new(10, 5);
        state.feed(b"\x1b[3;7H"); // move to row 3, col 7
        // CSI 6 n → Device Status Report
        state.feed(b"\x1b[6n");
        let responses = state.drain_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0], b"\x1b[3;7R");
    }

    #[test]
    fn device_attributes_responds() {
        let mut state = TerminalState::new(10, 5);
        state.feed(b"\x1b[c");
        let responses = state.drain_responses();
        assert_eq!(responses.len(), 1);
        assert!(responses[0].starts_with(b"\x1b[?"));
    }

    #[test]
    fn dec_private_cursor_position_report() {
        let mut state = TerminalState::new(10, 5);
        state.feed(b"\x1b[3;7H"); // move to row 3, col 7
        state.feed(b"\x1b[?6n");
        let responses = state.drain_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0], b"\x1b[?3;7R");
    }

    #[test]
    fn xtwinops_report_text_area_size() {
        let mut state = TerminalState::new(80, 24);
        state.feed(b"\x1b[18t");
        let responses = state.drain_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0], b"\x1b[8;24;80t");
    }

    #[test]
    fn insert_mode_shifts_characters_right() {
        let mut state = TerminalState::new(8, 1);
        state.feed(b"ABCDEF  ");
        // Enable IRM (Insert Replacement Mode)
        state.feed(b"\x1b[4h");
        // Move to column 3 and insert
        state.feed(b"\x1b[1;3HXY");
        assert_eq!(state.row_text(0), "ABXYCDEF");
        // Disable IRM
        state.feed(b"\x1b[4l");
        // Overwrite at column 1
        state.feed(b"\x1b[1;1HZ");
        assert_eq!(state.row_text(0), "ZBXYCDEF");
    }

    #[test]
    fn tmux_passthrough_forwards_wrapped_sequence_by_default() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\");

        let passthrough = state.drain_passthrough();
        assert_eq!(passthrough.len(), 1);
        assert_eq!(passthrough[0], b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(state.row_text(0), "        ");
    }

    #[test]
    fn tmux_passthrough_ignores_non_tmux_prefix() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1bPtest;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\");

        assert!(state.drain_passthrough().is_empty());
    }

    #[test]
    fn tmux_passthrough_can_be_disabled() {
        let mut state = TerminalState::new_with_passthrough(8, 2, false);
        state.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\");
        assert!(state.drain_passthrough().is_empty());
    }

    #[test]
    fn osc8_sequences_are_forwarded_to_passthrough_queue() {
        let mut state = TerminalState::new(16, 2);
        state.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07");

        let passthrough = state.drain_passthrough();
        assert_eq!(
            passthrough,
            vec![
                b"\x1b]8;;https://example.com\x07".to_vec(),
                b"\x1b]8;;\x07".to_vec()
            ]
        );
        assert_eq!(state.row_text(0), "link            ");
    }

    #[test]
    fn osc8_passthrough_respects_allow_passthrough_toggle() {
        let mut state = TerminalState::new_with_passthrough(16, 2, false);
        state.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07");
        assert!(state.drain_passthrough().is_empty());
    }

    #[test]
    fn osc_0_sets_title_event() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1b]0;build\x07");
        assert_eq!(
            state.drain_events(),
            vec![TerminalEvent::TitleChanged {
                title: Some("build".to_string())
            }]
        );
    }

    #[test]
    fn osc_2_empty_resets_title_event() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1b]2;\x07");
        assert_eq!(
            state.drain_events(),
            vec![TerminalEvent::TitleChanged { title: None }]
        );
    }

    #[test]
    fn osc_7_sets_cwd_event() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1b]7;file:///tmp/spectra%20dir\x07");
        assert_eq!(
            state.drain_events(),
            vec![TerminalEvent::CwdChanged {
                cwd: "/tmp/spectra dir".to_string()
            }]
        );
    }

    #[test]
    fn osc_title_ignores_invalid_utf8() {
        let mut state = TerminalState::new(8, 2);
        state.feed(b"\x1b]0;\xff\x07");
        assert!(state.drain_events().is_empty());
    }

    #[test]
    fn osc_title_strips_controls_and_truncates() {
        let mut state = TerminalState::new(8, 2);
        let long_title = "a".repeat(280);
        let sequence = format!("\x1b]0;ab\x01cd{long_title}\x07");
        state.feed(sequence.as_bytes());
        let events = state.drain_events();
        assert_eq!(events.len(), 1);
        let TerminalEvent::TitleChanged { title } = &events[0] else {
            panic!("expected title change event");
        };
        let title = title.as_ref().expect("title should exist");
        assert!(title.starts_with("abcd"));
        assert_eq!(title.len(), 256);
    }

    #[test]
    fn reflow_shrink_width_wraps_content() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"abcdef");
        state.resize(3, 2);

        assert_eq!(state.row_text(0), "abc");
        assert_eq!(state.row_text(1), "def");
    }

    #[test]
    fn reflow_expand_width_rejoins_soft_wrapped() {
        let mut state = TerminalState::new(3, 2);
        state.feed(b"abcdef");
        // Now: row0="abc"(SW), row1="def"(None)
        state.resize(6, 2);

        assert_eq!(state.row_text(0), "abcdef");
        assert_eq!(state.row_text(1), "      ");
    }

    #[test]
    fn reflow_preserves_hard_newlines() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"abc\r\ndef");
        state.resize(3, 2);

        // Hard newline separates logical lines — no joining
        assert_eq!(state.row_text(0), "abc");
        assert_eq!(state.row_text(1), "def");
    }

    #[test]
    fn reflow_hard_newline_not_merged_on_expand() {
        let mut state = TerminalState::new(3, 2);
        state.feed(b"abc\r\ndef");
        state.resize(6, 2);

        // Hard newline prevents joining into one row
        assert_eq!(state.row_text(0), "abc   ");
        assert_eq!(state.row_text(1), "def   ");
    }

    #[test]
    fn reflow_cursor_maps_correctly_on_shrink() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"abcdef");
        // cursor at (6, 0) pending wrap → clamped to (5, 0)
        state.resize(3, 2);

        // "abcdef" at width 3: "abc"(SW), "def"(None)
        // cursor was at col 5 → offset 5 in logical line → maps to (1, 2) in rewrap
        assert_eq!(state.cursor(), (2, 1));
    }

    #[test]
    fn reflow_cursor_maps_correctly_on_expand() {
        let mut state = TerminalState::new(3, 3);
        state.feed(b"abcde");
        // row0="abc"(SW), row1="de "(None), cursor at (2, 1)
        state.resize(6, 2);

        // Logical line: "abcde " → "abcde " at width 6, 1 row
        // cursor at offset 3 + 2 = 5 → col 5 in row 0
        assert_eq!(state.row_text(0), "abcde ");
        assert_eq!(state.cursor(), (5, 0));
    }

    #[test]
    fn reflow_scrollback_participates() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"line1\r\nline2\r\nline3");
        // "line1" scrolled to scrollback, visible: "line2", "line3"
        state.resize(12, 3);

        // After reflow at width 12, each line fits in 1 row.
        // Scrollback should be empty, all 3 lines visible.
        assert_eq!(state.row_text(0), "line1       ");
        assert_eq!(state.row_text(1), "line2       ");
        assert_eq!(state.row_text(2), "line3       ");
    }

    #[test]
    fn reflow_scrollback_content_reflows() {
        let mut state = TerminalState::new(6, 1);
        state.feed(b"abcdef\r\nX");
        // "abcdef" in scrollback (boundary HardLf), visible: "X     "
        state.resize(3, 4);

        // "abcdef" at width 3: "abc"(SW), "def"(HardLf) → 2 rows
        // "X     " at width 3: "X  "(None) → 1 row
        // Total 3 rows, height 4 → all visible + 1 blank
        assert_eq!(state.row_text(0), "abc");
        assert_eq!(state.row_text(1), "def");
        assert_eq!(state.row_text(2), "X  ");
    }

    #[test]
    fn reflow_alt_screen_no_reflow() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"abcdef");
        // Enter alt screen (DECSET 1049)
        state.feed(b"\x1b[?1049h");
        state.feed(b"XYZTOP");
        state.resize(3, 2);

        // Alt screen gets naive resize: top-left copy
        assert_eq!(state.row_text(0), "XYZ");
        assert_eq!(state.row_text(1), "   ");

        // Leave alt screen
        state.feed(b"\x1b[?1049l");

        // Primary screen was reflowed: "abcdef" → "abc"(SW), "def"(None)
        assert_eq!(state.row_text(0), "abc");
        assert_eq!(state.row_text(1), "def");
    }

    #[test]
    fn reflow_wide_char_wraps_at_boundary() {
        let mut state = TerminalState::new(3, 2);
        // Write "ab" then a wide char (Chinese character '中', 2 cols wide)
        state.feed("ab中".as_bytes());
        // width 3: 'a'(col0), 'b'(col1), col2 has only 1 space left.
        // '中' needs 2 cols → pad col2 with space, wrap '中' to row 1
        assert_eq!(state.row_text(0), "ab ");
        assert_eq!(state.row_text(1), "中 ");

        state.resize(4, 2);
        // Reflow: logical line = "ab " + "中 " (soft-wrapped) = "ab 中 "
        // At width 4: 'a'(0), 'b'(1), ' '(2), '中'(3,4)... '中' needs 2 cols
        // col 3, needs col 3+4 but width is 4, so col+2=5 > 4 → wrap
        // row 0: "ab  " (pad), row 1: "中  "
        // Actually: 'a'(col0), 'b'(col1), ' '(col2), then '中' at col3:
        // col+2=5 > 4 → doesn't fit. Pad to "ab  "(SW). New row: "中  "(None).
        assert_eq!(state.row_text(0), "ab  ");
        assert_eq!(state.row_text(1), "中  ");
    }

    #[test]
    fn reflow_same_dimensions_is_noop() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"hello\r\nworld");
        state.resize(6, 2);

        assert_eq!(state.row_text(0), "hello ");
        assert_eq!(state.row_text(1), "world ");
        assert_eq!(state.cursor(), (5, 1));
    }

    #[test]
    fn reflow_height_only_change_pulls_scrollback() {
        let mut state = TerminalState::new(6, 2);
        state.feed(b"line1\r\nline2\r\nline3");
        // "line1" in scrollback, visible: "line2", "line3"
        state.resize(6, 3);

        // Same width, more height → scrollback pulled back
        assert_eq!(state.row_text(0), "line1 ");
        assert_eq!(state.row_text(1), "line2 ");
        assert_eq!(state.row_text(2), "line3 ");
    }
}
