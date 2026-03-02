use std::io;

use crate::session::terminal_state::{StyledCell, TerminalEvent, TerminalState};

pub trait PaneBackend: Send {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
    fn poll_output(&mut self) -> Vec<Vec<u8>>;
    fn is_closed(&mut self) -> bool {
        false
    }
}

pub struct Pane {
    terminal: TerminalState,
    backend: Box<dyn PaneBackend>,
    view_scroll_offset: usize,
    pending_passthrough: Vec<Vec<u8>>,
    pending_terminal_events: Vec<TerminalEvent>,
}

impl Pane {
    pub fn new(
        width: usize,
        height: usize,
        allow_passthrough: bool,
        backend: Box<dyn PaneBackend>,
    ) -> Self {
        Self {
            terminal: TerminalState::new_with_passthrough(width, height, allow_passthrough),
            backend,
            view_scroll_offset: 0,
            pending_passthrough: Vec::new(),
            pending_terminal_events: Vec::new(),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.backend.write(bytes)
    }

    pub fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.terminal.resize(width, height);
        self.backend.resize(width as u16, height as u16)
    }

    pub fn scroll_view(&mut self, lines: isize, view_rows: usize) {
        let max_offset = self.max_view_scroll_offset(view_rows);
        if max_offset == 0 || lines == 0 {
            self.view_scroll_offset = 0;
            return;
        }

        if lines.is_negative() {
            self.view_scroll_offset = self.view_scroll_offset.saturating_sub(lines.unsigned_abs());
        } else {
            self.view_scroll_offset = self
                .view_scroll_offset
                .saturating_add(lines as usize)
                .min(max_offset);
        }
    }

    pub fn reset_view_scroll(&mut self) -> bool {
        if self.view_scroll_offset == 0 {
            return false;
        }
        self.view_scroll_offset = 0;
        true
    }

    pub fn poll_output(&mut self) -> bool {
        let mut changed = false;
        let preserve_view_origin = if self.view_scroll_offset > 0 {
            let view_rows = self.terminal.height().max(1);
            Some(self.view_row_origin(view_rows))
        } else {
            None
        };
        for chunk in self.backend.poll_output() {
            self.terminal.feed(&chunk);
            self.pending_passthrough
                .extend(self.terminal.drain_passthrough());
            self.pending_terminal_events
                .extend(self.terminal.drain_events());
            changed = true;
        }
        if changed && let Some(target_origin) = preserve_view_origin {
            let view_rows = self.terminal.height().max(1);
            let follow_origin = self.follow_row_origin(view_rows);
            let clamped_origin = target_origin.min(follow_origin);
            self.view_scroll_offset = follow_origin.saturating_sub(clamped_origin);
        }
        // Send any terminal responses (e.g. cursor position reports) back
        for response in self.terminal.drain_responses() {
            let _ = self.backend.write(&response);
        }
        changed
    }

    pub fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.terminal.set_allow_passthrough(allow_passthrough);
        if !allow_passthrough {
            self.pending_passthrough.clear();
        }
    }

    pub fn allow_passthrough(&self) -> bool {
        self.terminal.allow_passthrough()
    }

    pub fn take_passthrough(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_passthrough)
    }

    pub fn take_terminal_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.pending_terminal_events)
    }

    pub fn row_text(&self, row: usize) -> String {
        self.terminal.row_text(row)
    }

    pub fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        self.terminal.row_cells(row)
    }

    pub fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        self.terminal.absolute_row_cells(absolute_row)
    }

    pub fn total_lines(&self) -> usize {
        self.terminal.total_lines()
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.terminal.cursor()
    }

    pub fn cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        self.terminal.cursor_style()
    }

    pub fn scrollback_text(&self) -> String {
        self.terminal.scrollback_text()
    }

    pub fn history_lines(&self) -> Vec<String> {
        self.terminal.history_lines()
    }

    pub fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        self.terminal.history_cells()
    }

    pub fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        self.terminal.history_tail_lines(max_lines)
    }

    pub fn row_cells_for_view(&self, view_rows: usize) -> Vec<Vec<StyledCell>> {
        if view_rows == 0 {
            return Vec::new();
        }
        let row_origin = self.view_row_origin(view_rows);
        (0..view_rows)
            .map(|row| self.terminal.absolute_row_cells(row_origin + row))
            .collect()
    }

    pub fn cursor_row_in_view(&self, view_rows: usize) -> Option<usize> {
        if view_rows == 0 {
            return None;
        }
        let cursor_absolute_row = self.terminal.history_len() + self.terminal.cursor().1;
        let row_origin = self.view_row_origin(view_rows);

        if cursor_absolute_row < row_origin {
            return None;
        }

        let cursor_view_row = cursor_absolute_row - row_origin;
        (cursor_view_row < view_rows).then_some(cursor_view_row)
    }

    pub fn view_row_origin_for(&self, view_rows: usize) -> usize {
        self.view_row_origin(view_rows)
    }

    pub fn cursor_absolute_position(&self) -> (usize, usize) {
        let (col, row) = self.terminal.cursor();
        (col, self.terminal.history_len() + row)
    }

    fn max_view_scroll_offset(&self, view_rows: usize) -> usize {
        self.follow_row_origin(view_rows)
    }

    fn follow_row_origin(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        let history_len = self.terminal.history_len();
        let cursor_absolute_row = history_len + self.terminal.cursor().1;
        let max_origin = self.terminal.total_lines().saturating_sub(view_rows);
        cursor_absolute_row
            .saturating_add(1)
            .saturating_sub(view_rows)
            .max(history_len)
            .min(max_origin)
    }

    fn view_row_origin(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        let follow_origin = self.follow_row_origin(view_rows);
        let offset = self.view_scroll_offset.min(follow_origin);
        follow_origin.saturating_sub(offset)
    }

    pub fn export_text_hard_lf(&self) -> String {
        self.terminal.export_text_hard_lf()
    }

    pub fn is_closed(&mut self) -> bool {
        self.backend.is_closed()
    }
}

pub struct FakeBackend {
    output: Vec<Vec<u8>>,
    pub writes: Vec<Vec<u8>>,
    pub last_size: Option<(u16, u16)>,
}

impl FakeBackend {
    pub fn new(output: Vec<Vec<u8>>) -> Self {
        Self {
            output,
            writes: Vec::new(),
            last_size: None,
        }
    }
}

impl PaneBackend for FakeBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.last_size = Some((cols, rows));
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.output)
    }
}
