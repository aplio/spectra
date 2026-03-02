use std::collections::HashMap;
use std::io::{self, Write};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use crossterm::{
    cursor::{self, MoveTo, SetCursorStyle},
    queue,
    style::{Color, Print},
    terminal::{Clear, ClearType},
};

use unicode_width::UnicodeWidthChar;

use crate::session::manager::RenderFrame;
use crate::session::terminal_state::{CellStyle, StyledCell};
use crate::ui::window_manager::{Divider, DividerOrientation};
use crate::ui::style::{apply_style, reset_style};
use crate::ui::text::{display_width, truncate_to_width};
use crate::ui::url::{
    UrlSpan, find_web_url_spans, write_hyperlink_close, write_hyperlink_open,
};

#[derive(Debug, Clone)]
pub struct SystemOverlay {
    pub title: String,
    pub query: String,
    pub query_cursor_pos: usize,
    pub query_active: bool,
    pub candidates: Vec<String>,
    pub selected: usize,
    pub selected_cursor_pos: Option<usize>,
    pub preview_lines: Vec<String>,
    pub preview_from_tail: bool,
}

#[derive(Debug, Clone)]
pub struct SideWindowTree {
    pub title: String,
    pub entries: Vec<String>,
    pub selected: usize,
    pub width: usize,
}

pub struct FrameRenderer {
    previous: Option<BackBuffer>,
}

#[derive(Debug, Clone)]
struct BackBuffer {
    cols: u16,
    rows: u16,
    cells: Vec<StyledCell>,
}

impl BackBuffer {
    fn from_composed(frame: &ComposedFrame) -> Self {
        Self {
            cols: frame.cols,
            rows: frame.rows,
            cells: frame.cells.clone(),
        }
    }

    fn matches_dimensions(&self, frame: &ComposedFrame) -> bool {
        self.cols == frame.cols && self.rows == frame.rows
    }
}

#[derive(Debug, Clone)]
struct ComposedFrame {
    cols: u16,
    rows: u16,
    cells: Vec<StyledCell>,
    cursor: (u16, u16),
    cursor_style: SetCursorStyle,
}

impl ComposedFrame {
    fn new(cols: u16, rows: u16) -> Self {
        let width = usize::from(cols);
        let height = usize::from(rows);
        Self {
            cols,
            rows,
            cells: vec![StyledCell::default(); width * height],
            cursor: (0, 0),
            cursor_style: SetCursorStyle::DefaultUserShape,
        }
    }

    fn width(&self) -> usize {
        usize::from(self.cols)
    }

    fn height(&self) -> usize {
        usize::from(self.rows)
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.width() + x
    }

    fn set(&mut self, x: usize, y: usize, cell: StyledCell) {
        if x >= self.width() || y >= self.height() {
            return;
        }
        let idx = self.idx(x, y);
        self.cells[idx] = cell;
    }

    fn set_fg(&mut self, x: usize, y: usize, color: Color) {
        if x >= self.width() || y >= self.height() {
            return;
        }
        let idx = self.idx(x, y);
        self.cells[idx].style.fg = Some(color);
    }

    fn row_slice(&self, row: usize) -> &[StyledCell] {
        let width = self.width();
        let start = row * width;
        let end = start + width;
        &self.cells[start..end]
    }
}

impl Default for FrameRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameRenderer {
    pub fn new() -> Self {
        Self { previous: None }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_to_writer<W: Write>(
        &mut self,
        writer: &mut W,
        frame: &RenderFrame,
        status_line: &str,
        cols: u16,
        rows: u16,
        full_clear: bool,
        overlay: Option<&SystemOverlay>,
        side_window_tree: Option<&SideWindowTree>,
    ) -> io::Result<()> {
        self.render_to_writer_with_status_style(
            writer,
            frame,
            status_line,
            CellStyle::default(),
            cols,
            rows,
            full_clear,
            overlay,
            side_window_tree,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_to_writer_with_status_style<W: Write>(
        &mut self,
        writer: &mut W,
        frame: &RenderFrame,
        status_line: &str,
        status_style: CellStyle,
        cols: u16,
        rows: u16,
        full_clear: bool,
        overlay: Option<&SystemOverlay>,
        side_window_tree: Option<&SideWindowTree>,
    ) -> io::Result<()> {
        let composed = compose_frame(
            frame,
            status_line,
            status_style,
            cols,
            rows,
            overlay,
            side_window_tree,
        );
        let previous = self
            .previous
            .as_ref()
            .filter(|previous| previous.matches_dimensions(&composed));

        queue!(writer, cursor::Hide)?;
        if full_clear {
            queue!(writer, MoveTo(0, 0), Clear(ClearType::All))?;
            self.emit_full(writer, &composed)?;
        } else if let Some(previous) = previous {
            self.emit_diff(writer, previous, &composed)?;
        } else {
            self.emit_full(writer, &composed)?;
        }

        reset_style(writer)?;
        queue!(
            writer,
            MoveTo(composed.cursor.0, composed.cursor.1),
            composed.cursor_style,
            cursor::Show
        )?;
        writer.flush()?;

        self.previous = Some(BackBuffer::from_composed(&composed));
        Ok(())
    }

    fn emit_full<W: Write>(&self, writer: &mut W, frame: &ComposedFrame) -> io::Result<()> {
        for y in 0..frame.height() {
            queue!(writer, MoveTo(0, y as u16))?;
            write_styled_cells(writer, frame.row_slice(y), 0)?;
        }
        Ok(())
    }

    fn emit_diff<W: Write>(
        &self,
        writer: &mut W,
        previous: &BackBuffer,
        frame: &ComposedFrame,
    ) -> io::Result<()> {
        let width = frame.width();
        let height = frame.height();

        for y in 0..height {
            let row_offset = y * width;
            let row_end = row_offset + width;
            let previous_row = &previous.cells[row_offset..row_end];
            let current_row = &frame.cells[row_offset..row_end];
            let previous_row_urls = RowUrlMap::from_row(previous_row);
            let current_row_urls = RowUrlMap::from_row(current_row);

            let Some(mut start) = (0..width).position(|col| {
                previous_row[col] != current_row[col]
                    || previous_row_urls.url_key_for_col(col)
                        != current_row_urls.url_key_for_col(col)
            }) else {
                continue;
            };

            if current_row[start].ch == '\0' && start > 0 {
                start -= 1;
            }

            queue!(writer, MoveTo(start as u16, y as u16))?;
            write_styled_cells(writer, current_row, start)?;
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_to_writer<W: Write>(
    writer: &mut W,
    frame: &RenderFrame,
    status_line: &str,
    cols: u16,
    rows: u16,
    full_clear: bool,
    overlay: Option<&SystemOverlay>,
    side_window_tree: Option<&SideWindowTree>,
) -> io::Result<()> {
    let mut renderer = FrameRenderer::new();
    renderer.render_to_writer(
        writer,
        frame,
        status_line,
        cols,
        rows,
        full_clear,
        overlay,
        side_window_tree,
    )
}

fn compose_frame(
    frame: &RenderFrame,
    status_line: &str,
    status_style: CellStyle,
    cols: u16,
    rows: u16,
    overlay: Option<&SystemOverlay>,
    side_window_tree: Option<&SideWindowTree>,
) -> ComposedFrame {
    let mut composed = ComposedFrame::new(cols, rows);
    let workspace_rows = usize::from(rows.saturating_sub(1));

    for pane in &frame.panes {
        for rel_y in 0..pane.rect.height {
            let y = pane.rect.y + rel_y;
            if y >= workspace_rows {
                continue;
            }
            let line = pane.rows.get(rel_y).map(Vec::as_slice).unwrap_or(&[]);
            let display = fixed_width_cells(line, pane.rect.width);
            for (x, cell) in display.into_iter().enumerate() {
                composed.set(pane.rect.x + x, y, cell);
            }
        }
    }

    let divider_cells = connected_divider_cells(&frame.dividers, usize::from(cols), workspace_rows);
    for ((x, y), ch) in divider_cells {
        composed.set(
            x,
            y,
            StyledCell {
                ch,
                ..StyledCell::default()
            },
        );
    }

    style_focused_pane_dividers(&mut composed, frame, workspace_rows);
    compose_side_window_tree(&mut composed, side_window_tree, workspace_rows);

    let overlay_cursor = overlay.and_then(|overlay| compose_overlay(&mut composed, overlay));
    let status_y = usize::from(rows.saturating_sub(1));
    draw_text_with_style(
        &mut composed,
        0,
        status_y,
        &fixed_width(status_line, usize::from(cols)),
        status_style,
    );

    let status_cursor_y = rows.saturating_sub(1);
    let cursor = if let Some((x, y)) = overlay_cursor {
        (x, y)
    } else if let Some((x, y)) = frame.focused_cursor {
        if y < status_cursor_y {
            (x, y)
        } else {
            (0, status_cursor_y)
        }
    } else {
        (0, status_cursor_y)
    };
    composed.cursor = clamp_cursor(cursor, cols, rows);
    composed.cursor_style = frame.cursor_style;

    composed
}

fn compose_side_window_tree(
    frame: &mut ComposedFrame,
    side_window_tree: Option<&SideWindowTree>,
    workspace_rows: usize,
) {
    let Some(side) = side_window_tree else {
        return;
    };
    if workspace_rows == 0 {
        return;
    }

    let total_cols = usize::from(frame.cols);
    let mut width = side.width.min(total_cols.saturating_sub(1));
    if width < 4 {
        return;
    }
    if width > total_cols {
        width = total_cols;
    }

    let divider_x = width.saturating_sub(1);
    let content_w = width.saturating_sub(1);
    let header = fixed_width(&side.title, content_w);
    draw_text_with_style(
        frame,
        0,
        0,
        &header,
        CellStyle {
            dim: true,
            ..CellStyle::default()
        },
    );
    for y in 1..workspace_rows {
        frame.set(
            divider_x,
            y,
            StyledCell {
                ch: '│',
                ..StyledCell::default()
            },
        );
    }
    frame.set(
        divider_x,
        0,
        StyledCell {
            ch: '│',
            style: CellStyle {
                dim: true,
                ..CellStyle::default()
            },
        },
    );

    let content_h = workspace_rows.saturating_sub(1);
    if content_h == 0 {
        return;
    }

    let selected = side.selected.min(side.entries.len().saturating_sub(1));
    let start = scroll_start(selected, side.entries.len(), content_h);
    for row in 0..content_h {
        let entry_idx = start + row;
        let text = side
            .entries
            .get(entry_idx)
            .map(String::as_str)
            .unwrap_or_default();
        let y = 1 + row;
        let is_selected = entry_idx == selected && !side.entries.is_empty();
        let line = if is_selected {
            format!("> {text}")
        } else {
            format!("  {text}")
        };
        draw_text_with_style(
            frame,
            0,
            y,
            &fixed_width(&line, content_w),
            if is_selected {
                CellStyle {
                    reverse: true,
                    ..CellStyle::default()
                }
            } else {
                CellStyle::default()
            },
        );
    }
}

fn focused_pane_border_color() -> Color {
    Color::Cyan
}

fn style_focused_pane_dividers(
    frame: &mut ComposedFrame,
    render: &RenderFrame,
    workspace_rows: usize,
) {
    if render.panes.len() <= 1 {
        return;
    }

    let Some(pane) = render.panes.iter().find(|pane| pane.focused) else {
        return;
    };
    if pane.rect.width == 0 || pane.rect.height == 0 {
        return;
    }

    let border_color = focused_pane_border_color();
    let pane_left = pane.rect.x;
    let pane_right = pane.rect.x.saturating_add(pane.rect.width);
    let pane_top = pane.rect.y;
    let pane_bottom = pane.rect.y.saturating_add(pane.rect.height);

    for divider in &render.dividers {
        match divider.orientation {
            DividerOrientation::Vertical => {
                let touches_left = pane_left > 0 && divider.x.saturating_add(1) == pane_left;
                let touches_right = divider.x == pane_right;
                if !touches_left && !touches_right {
                    continue;
                }

                let divider_start = divider.y;
                let divider_end = divider.y.saturating_add(divider.len);
                let y_start = pane_top.max(divider_start);
                let y_end = pane_bottom.min(divider_end);
                for y in y_start..y_end {
                    if y < workspace_rows {
                        frame.set_fg(divider.x, y, border_color);
                    }
                }
            }
            DividerOrientation::Horizontal => {
                let touches_top = pane_top > 0 && divider.y.saturating_add(1) == pane_top;
                let touches_bottom = divider.y == pane_bottom;
                if !touches_top && !touches_bottom {
                    continue;
                }

                let divider_start = divider.x;
                let divider_end = divider.x.saturating_add(divider.len);
                let x_start = pane_left.max(divider_start);
                let x_end = pane_right.min(divider_end);
                if divider.y >= workspace_rows {
                    continue;
                }
                for x in x_start..x_end {
                    frame.set_fg(x, divider.y, border_color);
                }
            }
        }
    }
}

fn compose_overlay(frame: &mut ComposedFrame, overlay: &SystemOverlay) -> Option<(u16, u16)> {
    let workspace_rows = usize::from(frame.rows.saturating_sub(1));
    let total_cols = usize::from(frame.cols);
    if workspace_rows < 4 || total_cols < 20 {
        return None;
    }

    let min_split_width = 48usize;
    let min_split_height = 9usize;
    if total_cols < min_split_width || workspace_rows < min_split_height {
        return compose_overlay_compact(frame, overlay);
    }

    let popup_width = total_cols.saturating_sub(2).max(40).min(total_cols);
    let popup_height = workspace_rows.max(8).min(workspace_rows);
    if popup_width < 26 || popup_height < 8 {
        return compose_overlay_compact(frame, overlay);
    }

    let popup_x = total_cols.saturating_sub(popup_width) / 2;
    let popup_y = workspace_rows.saturating_sub(popup_height) / 2;
    let inner_width = popup_width.saturating_sub(2);
    let inner_height = popup_height.saturating_sub(2);

    let horizontal_gap = 1usize;
    if inner_width <= 30 || inner_height <= 6 {
        return compose_overlay_compact(frame, overlay);
    }
    let mut left_width = (inner_width * 45) / 100;
    left_width = left_width.clamp(18, inner_width.saturating_sub(horizontal_gap + 16));
    let right_width = inner_width.saturating_sub(horizontal_gap + left_width);
    if right_width < 16 {
        return compose_overlay_compact(frame, overlay);
    }

    let left_x = popup_x + 1;
    let right_x = left_x + left_width + horizontal_gap;
    let top_y = popup_y + 1;

    let vertical_gap = 1usize;
    let input_height = 3usize;
    if inner_height <= input_height + vertical_gap + 2 {
        return compose_overlay_compact(frame, overlay);
    }
    let candidate_height = inner_height - input_height - vertical_gap;
    if candidate_height < 3 {
        return compose_overlay_compact(frame, overlay);
    }

    draw_box(
        frame,
        popup_x,
        popup_y,
        popup_width,
        popup_height,
        &overlay.title,
    );

    draw_box(frame, left_x, top_y, left_width, input_height, "input");
    draw_box(
        frame,
        left_x,
        top_y + input_height + vertical_gap,
        left_width,
        candidate_height,
        "candidates",
    );
    draw_box(frame, right_x, top_y, right_width, inner_height, "preview");

    let input_inner_w = left_width.saturating_sub(2);
    let input_text = format!("/{}", overlay.query);
    draw_text(
        frame,
        left_x + 1,
        top_y + 1,
        &fixed_width(&input_text, input_inner_w),
    );
    let query_cursor_pos = overlay.query_cursor_pos.min(overlay.query.chars().count());
    let cursor_display_width: usize = overlay
        .query
        .chars()
        .take(query_cursor_pos)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let input_cursor_col = (1 + cursor_display_width).min(input_inner_w);
    let input_cursor_x = left_x + 1 + input_cursor_col;
    let input_cursor = Some((input_cursor_x as u16, (top_y + 1) as u16));

    let candidate_inner_w = left_width.saturating_sub(2);
    let candidate_content_h = candidate_height.saturating_sub(2);
    let candidate_count = overlay.candidates.len();
    let selected = overlay.selected.min(candidate_count.saturating_sub(1));
    let start = scroll_start(selected, candidate_count, candidate_content_h);
    let mut candidate_cursor = None;
    for row in 0..candidate_content_h {
        let candidate_idx = start + row;
        let content = overlay
            .candidates
            .get(candidate_idx)
            .map(String::as_str)
            .unwrap_or_default();
        let marked = if candidate_idx == selected && candidate_count > 0 {
            format!("> {content}")
        } else {
            format!("  {content}")
        };
        let y = top_y + input_height + vertical_gap + 1 + row;
        draw_text(
            frame,
            left_x + 1,
            y,
            &fixed_width(&marked, candidate_inner_w),
        );
        if candidate_idx == selected && candidate_count > 0 {
            let cursor_col = if let Some(cursor_pos) = overlay.selected_cursor_pos {
                let cursor_chars = cursor_pos.min(content.chars().count());
                let content_cursor_width: usize = content
                    .chars()
                    .take(cursor_chars)
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum();
                (2 + content_cursor_width).min(candidate_inner_w.saturating_sub(1))
            } else {
                2.min(candidate_inner_w.saturating_sub(1))
            };
            let cursor_x = left_x + 1 + cursor_col;
            candidate_cursor = Some((cursor_x as u16, y as u16));
        }
    }

    let preview_inner_w = right_width.saturating_sub(2);
    let preview_content_h = inner_height.saturating_sub(2);
    let preview_start = if overlay.preview_from_tail {
        overlay
            .preview_lines
            .len()
            .saturating_sub(preview_content_h)
    } else {
        0
    };
    for row in 0..preview_content_h {
        let content = overlay
            .preview_lines
            .get(preview_start + row)
            .map(String::as_str)
            .unwrap_or_default();
        draw_text(
            frame,
            right_x + 1,
            top_y + 1 + row,
            &fixed_width(content, preview_inner_w),
        );
    }

    if overlay.query_active {
        input_cursor
    } else if candidate_count > 0 {
        candidate_cursor
    } else {
        input_cursor
    }
}

fn compose_overlay_compact(
    frame: &mut ComposedFrame,
    overlay: &SystemOverlay,
) -> Option<(u16, u16)> {
    let workspace_rows = usize::from(frame.rows.saturating_sub(1));
    let total_cols = usize::from(frame.cols);
    if workspace_rows < 3 || total_cols < 4 {
        return None;
    }

    let max_inner_width = total_cols.saturating_sub(4);
    if max_inner_width == 0 {
        return None;
    }

    let include_query_row = overlay.query_active || !overlay.query.is_empty();
    let mut lines = Vec::with_capacity(overlay.candidates.len() + usize::from(include_query_row));
    if include_query_row {
        lines.push(format!("/{}", overlay.query));
    }
    lines.extend(overlay.candidates.iter().cloned());

    let selected_line = if overlay.candidates.is_empty() {
        include_query_row.then_some(0)
    } else {
        Some(
            overlay
                .selected
                .min(overlay.candidates.len().saturating_sub(1))
                + usize::from(include_query_row),
        )
    };

    let line_count = lines.len().max(1);
    let max_body_rows = workspace_rows.saturating_sub(2).max(1);
    let body_rows = line_count.min(max_body_rows);
    let popup_height = body_rows + 2;

    let content_width = lines
        .iter()
        .map(|line| display_width(line) + 2)
        .max()
        .unwrap_or(2)
        .max(display_width(&overlay.title) + 2)
        .max(20);
    let inner_width = content_width.min(max_inner_width);
    let popup_width = inner_width + 2;
    let popup_x = total_cols.saturating_sub(popup_width) / 2;
    let popup_y = workspace_rows.saturating_sub(popup_height) / 2;

    draw_box(
        frame,
        popup_x,
        popup_y,
        popup_width,
        popup_height,
        &overlay.title,
    );

    let selected_anchor = selected_line.unwrap_or(0);
    let max_start = line_count.saturating_sub(body_rows);
    let mut start = selected_anchor.saturating_add(1).saturating_sub(body_rows);
    start = start.min(max_start);

    let mut cursor = None;
    for body_row in 0..body_rows {
        let line_index = start + body_row;
        let content = lines
            .get(line_index)
            .map(String::as_str)
            .unwrap_or_default();
        let marked = if include_query_row && line_index == 0 {
            format!("? {content}")
        } else if Some(line_index) == selected_line {
            format!("> {content}")
        } else {
            format!("  {content}")
        };
        let y = popup_y + 1 + body_row;
        draw_text(frame, popup_x + 1, y, &fixed_width(&marked, inner_width));

        if overlay.query_active && include_query_row && line_index == 0 {
            let query_cursor_pos = overlay.query_cursor_pos.min(overlay.query.chars().count());
            let cursor_display_width: usize = overlay
                .query
                .chars()
                .take(query_cursor_pos)
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            let cursor_col = (1 + cursor_display_width).min(inner_width.saturating_sub(1));
            let cursor_x = popup_x + 2 + cursor_col;
            cursor = Some((cursor_x as u16, y as u16));
        } else if !overlay.query_active && Some(line_index) == selected_line {
            let cursor_col = if let Some(cursor_pos) = overlay.selected_cursor_pos {
                let cursor_chars = cursor_pos.min(content.chars().count());
                let content_cursor_width: usize = content
                    .chars()
                    .take(cursor_chars)
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum();
                (2 + content_cursor_width).min(inner_width.saturating_sub(1))
            } else {
                2.min(inner_width.saturating_sub(1))
            };
            let cursor_x = popup_x + 1 + cursor_col;
            cursor = Some((cursor_x as u16, y as u16));
        }
    }

    cursor
}

fn draw_box(
    frame: &mut ComposedFrame,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    title: &str,
) {
    if width < 2 || height < 2 {
        return;
    }
    let inner_width = width - 2;
    let top = format!("┌{}┐", titled_inner(title, inner_width));
    let bottom = format!("└{}┘", "─".repeat(inner_width));
    draw_text(frame, x, y, &top);
    for row in 0..height.saturating_sub(2) {
        draw_text(
            frame,
            x,
            y + 1 + row,
            &format!("│{}│", " ".repeat(inner_width)),
        );
    }
    draw_text(frame, x, y + height - 1, &bottom);
}

fn titled_inner(title: &str, inner_width: usize) -> String {
    if title.is_empty() {
        return "─".repeat(inner_width);
    }
    let title_text = format!(" {} ", title);
    let title_width = display_width(&title_text);
    if title_width >= inner_width {
        let (truncated, _) = truncate_to_width(&title_text, inner_width);
        truncated.to_string()
    } else {
        let left = (inner_width - title_width) / 2;
        let right = inner_width - title_width - left;
        format!("{}{}{}", "─".repeat(left), title_text, "─".repeat(right))
    }
}

fn scroll_start(selected: usize, total: usize, visible: usize) -> usize {
    if total == 0 || visible == 0 || total <= visible {
        return 0;
    }
    let max_start = total - visible;
    selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(max_start)
}

fn clamp_cursor(cursor: (u16, u16), cols: u16, rows: u16) -> (u16, u16) {
    let max_x = cols.saturating_sub(1);
    let max_y = rows.saturating_sub(1);
    (cursor.0.min(max_x), cursor.1.min(max_y))
}

fn draw_text(frame: &mut ComposedFrame, x: usize, y: usize, text: &str) {
    draw_text_with_style(frame, x, y, text, CellStyle::default());
}

fn draw_text_with_style(
    frame: &mut ComposedFrame,
    x: usize,
    y: usize,
    text: &str,
    style: CellStyle,
) {
    let mut col = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        frame.set(x + col, y, StyledCell { ch, style });
        if w == 2 {
            frame.set(x + col + 1, y, StyledCell { ch: '\0', style });
        }
        col += w;
    }
}

fn fixed_width(input: &str, width: usize) -> String {
    let (truncated, used) = truncate_to_width(input, width);
    let mut out = truncated.to_string();
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

fn fixed_width_cells(cells: &[StyledCell], width: usize) -> Vec<StyledCell> {
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

struct RowUrlMap {
    text: String,
    spans: Vec<UrlSpan>,
    byte_by_col: Vec<Option<usize>>,
    url_key_by_col: Vec<Option<UrlKey>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UrlKey {
    start: usize,
    end: usize,
    hash: u64,
}

impl UrlKey {
    fn from_url_span(span: UrlSpan, text: &str) -> Self {
        let url = span.as_str(text);
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        Self {
            start: span.start,
            end: span.end,
            hash: hasher.finish(),
        }
    }
}

impl RowUrlMap {
    fn from_row(row: &[StyledCell]) -> Self {
        let mut text = String::new();
        let mut byte_by_col = vec![None; row.len()];
        for (idx, cell) in row.iter().enumerate() {
            if cell.ch == '\0' {
                continue;
            }
            byte_by_col[idx] = Some(text.len());
            text.push(cell.ch);
        }
        let spans = find_web_url_spans(&text);
        let span_keys = spans
            .iter()
            .map(|span| UrlKey::from_url_span(*span, &text))
            .collect::<Vec<_>>();
        let mut url_key_by_col = vec![None; row.len()];
        for (col, slot) in url_key_by_col.iter_mut().enumerate() {
            let Some(byte) = byte_by_col[col] else {
                continue;
            };
            if let Some((idx, _)) = spans
                .iter()
                .enumerate()
                .find(|(_, span)| span.contains_byte(byte))
            {
                *slot = Some(span_keys[idx]);
            }
        }
        Self {
            text,
            spans,
            byte_by_col,
            url_key_by_col,
        }
    }

    fn url_for_col(&self, col: usize) -> Option<&str> {
        let byte = self.byte_by_col.get(col).copied().flatten()?;
        self.spans
            .iter()
            .find(|span| span.contains_byte(byte))
            .map(|span| span.as_str(&self.text))
    }

    fn url_key_for_col(&self, col: usize) -> Option<UrlKey> {
        self.url_key_by_col.get(col).copied().flatten()
    }
}

fn write_styled_cells<W: Write>(
    writer: &mut W,
    row: &[StyledCell],
    start_col: usize,
) -> io::Result<()> {
    if row.is_empty() || start_col >= row.len() {
        return Ok(());
    }

    // Each call may follow a prior chunk that ended in a non-default SGR state.
    // Re-baseline style so default-styled cells do not inherit stale attributes.
    reset_style(writer)?;
    let mut current_style = CellStyle::default();
    let row_urls = RowUrlMap::from_row(row);
    let mut active_url: Option<String> = None;
    let mut run = String::new();

    for (idx, cell) in row.iter().enumerate().skip(start_col) {
        if cell.ch == '\0' {
            continue; // skip wide char continuation cell
        }
        let url_for_cell = row_urls.url_for_col(idx);
        if cell.style != current_style || active_url.as_deref() != url_for_cell {
            if !run.is_empty() {
                queue!(writer, Print(run.as_str()))?;
                run.clear();
            }
            if active_url.as_deref() != url_for_cell {
                if active_url.take().is_some() {
                    write_hyperlink_close(writer)?;
                }
                if let Some(url) = url_for_cell {
                    write_hyperlink_open(writer, url)?;
                    active_url = Some(url.to_string());
                }
            }
            if cell.style != current_style {
                apply_style(writer, cell.style)?;
                current_style = cell.style;
            }
        }
        run.push(cell.ch);
    }

    if !run.is_empty() {
        queue!(writer, Print(run.as_str()))?;
    }
    if active_url.is_some() {
        write_hyperlink_close(writer)?;
    }

    Ok(())
}

const UP: u8 = 0b0001;
const RIGHT: u8 = 0b0010;
const DOWN: u8 = 0b0100;
const LEFT: u8 = 0b1000;

fn connected_divider_cells(
    dividers: &[Divider],
    max_cols: usize,
    max_rows: usize,
) -> Vec<((usize, usize), char)> {
    let mut connections: HashMap<(usize, usize), u8> = HashMap::new();

    for divider in dividers {
        match divider.orientation {
            DividerOrientation::Vertical => {
                for dy in 0..divider.len.saturating_sub(1) {
                    let y0 = divider.y + dy;
                    let y1 = y0 + 1;
                    let x = divider.x;
                    if x >= max_cols || y0 >= max_rows || y1 >= max_rows {
                        continue;
                    }
                    *connections.entry((x, y0)).or_default() |= DOWN;
                    *connections.entry((x, y1)).or_default() |= UP;
                }

                if divider.len == 1 {
                    let x = divider.x;
                    let y = divider.y;
                    if x < max_cols && y < max_rows {
                        *connections.entry((x, y)).or_default() |= UP | DOWN;
                    }
                }
            }
            DividerOrientation::Horizontal => {
                for dx in 0..divider.len.saturating_sub(1) {
                    let x0 = divider.x + dx;
                    let x1 = x0 + 1;
                    let y = divider.y;
                    if y >= max_rows || x0 >= max_cols || x1 >= max_cols {
                        continue;
                    }
                    *connections.entry((x0, y)).or_default() |= RIGHT;
                    *connections.entry((x1, y)).or_default() |= LEFT;
                }

                if divider.len == 1 {
                    let x = divider.x;
                    let y = divider.y;
                    if x < max_cols && y < max_rows {
                        *connections.entry((x, y)).or_default() |= LEFT | RIGHT;
                    }
                }
            }
        }
    }

    bridge_adjacent_endpoint_junctions(&mut connections);
    bridge_single_cell_line_gaps(&mut connections);

    let mut cells = connections
        .into_iter()
        .map(|(coord, mask)| (coord, divider_glyph(mask)))
        .collect::<Vec<_>>();
    cells.sort_by_key(|((x, y), _)| (*y, *x));
    cells
}

fn bridge_adjacent_endpoint_junctions(connections: &mut HashMap<(usize, usize), u8>) {
    let mut patches = Vec::new();

    for (&(x, y), &mask) in connections.iter() {
        if is_vertical_only(mask) {
            if let Some(below_y) = y.checked_add(1)
                && let Some(&below_mask) = connections.get(&(x, below_y))
                && is_horizontal_only(below_mask)
            {
                patches.push(((x, y), DOWN));
                patches.push(((x, below_y), UP));
            }

            if y > 0
                && let Some(&above_mask) = connections.get(&(x, y - 1))
                && is_horizontal_only(above_mask)
            {
                patches.push(((x, y), UP));
                patches.push(((x, y - 1), DOWN));
            }
        }

        if is_horizontal_only(mask) {
            if let Some(right_x) = x.checked_add(1)
                && let Some(&right_mask) = connections.get(&(right_x, y))
                && is_vertical_only(right_mask)
            {
                patches.push(((x, y), RIGHT));
                patches.push(((right_x, y), LEFT));
            }

            if x > 0
                && let Some(&left_mask) = connections.get(&(x - 1, y))
                && is_vertical_only(left_mask)
            {
                patches.push(((x, y), LEFT));
                patches.push(((x - 1, y), RIGHT));
            }
        }
    }

    for (coord, patch) in patches {
        *connections.entry(coord).or_default() |= patch;
    }
}

fn bridge_single_cell_line_gaps(connections: &mut HashMap<(usize, usize), u8>) {
    const MAX_GAP_CELLS: usize = 3;
    let mut patches = Vec::new();

    for (&(x, y), &mask) in connections.iter() {
        if has_vertical(mask) {
            for gap_cells in 1..=MAX_GAP_CELLS {
                let span = gap_cells + 1;
                let Some(y2) = y.checked_add(span) else {
                    continue;
                };
                let Some(&far_mask) = connections.get(&(x, y2)) else {
                    continue;
                };
                if !has_vertical(far_mask) {
                    continue;
                }

                let has_intermediate_vertical = (1..span).any(|offset| {
                    connections
                        .get(&(x, y + offset))
                        .is_some_and(|mid_mask| has_vertical(*mid_mask))
                });
                if has_intermediate_vertical {
                    continue;
                }

                patches.push(((x, y), DOWN));
                for offset in 1..span {
                    patches.push(((x, y + offset), UP | DOWN));
                }
                patches.push(((x, y2), UP));
                break;
            }
        }

        if has_horizontal(mask) {
            for gap_cells in 1..=MAX_GAP_CELLS {
                let span = gap_cells + 1;
                let Some(x2) = x.checked_add(span) else {
                    continue;
                };
                let Some(&far_mask) = connections.get(&(x2, y)) else {
                    continue;
                };
                if !has_horizontal(far_mask) {
                    continue;
                }

                let has_intermediate_horizontal = (1..span).any(|offset| {
                    connections
                        .get(&(x + offset, y))
                        .is_some_and(|mid_mask| has_horizontal(*mid_mask))
                });
                if has_intermediate_horizontal {
                    continue;
                }

                patches.push(((x, y), RIGHT));
                for offset in 1..span {
                    patches.push(((x + offset, y), LEFT | RIGHT));
                }
                patches.push(((x2, y), LEFT));
                break;
            }
        }
    }

    for (coord, patch) in patches {
        *connections.entry(coord).or_default() |= patch;
    }
}

fn has_vertical(mask: u8) -> bool {
    mask & (UP | DOWN) != 0
}

fn has_horizontal(mask: u8) -> bool {
    mask & (LEFT | RIGHT) != 0
}

fn is_vertical_only(mask: u8) -> bool {
    has_vertical(mask) && !has_horizontal(mask)
}

fn is_horizontal_only(mask: u8) -> bool {
    has_horizontal(mask) && !has_vertical(mask)
}

fn divider_glyph(mask: u8) -> char {
    let up = mask & UP != 0;
    let right = mask & RIGHT != 0;
    let down = mask & DOWN != 0;
    let left = mask & LEFT != 0;

    match (up, right, down, left) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '├',
        (true, false, true, true) => '┤',
        (false, true, true, true) => '┬',
        (true, true, false, true) => '┴',
        (true, false, true, false) => '│',
        (false, true, false, true) => '─',
        (true, true, false, false) => '└',
        (true, false, false, true) => '┘',
        (false, true, true, false) => '┌',
        (false, false, true, true) => '┐',
        (true, false, false, false) | (false, false, true, false) => '│',
        (false, true, false, false) | (false, false, false, true) => '─',
        _ => ' ',
    }
}

#[cfg(test)]
mod tests {
    use crossterm::cursor::SetCursorStyle;
    use crossterm::style::Color;

    use crate::session::manager::{RenderFrame, RenderPane};
    use crate::session::terminal_state::{CellStyle, StyledCell};
    use crate::ui::window_manager::{Divider, DividerOrientation, PaneRect};

    use super::{
        DOWN, FrameRenderer, LEFT, RIGHT, SideWindowTree, UP, compose_frame,
        connected_divider_cells, divider_glyph, fixed_width_cells, focused_pane_border_color,
        write_styled_cells,
    };

    #[test]
    fn divider_glyphs_cover_common_masks() {
        assert_eq!(divider_glyph(UP | DOWN), '│');
        assert_eq!(divider_glyph(LEFT | RIGHT), '─');
        assert_eq!(divider_glyph(UP | RIGHT | DOWN | LEFT), '┼');
        assert_eq!(divider_glyph(UP | RIGHT | DOWN), '├');
        assert_eq!(divider_glyph(UP | DOWN | LEFT), '┤');
        assert_eq!(divider_glyph(RIGHT | DOWN | LEFT), '┬');
        assert_eq!(divider_glyph(UP | RIGHT | LEFT), '┴');
    }

    #[test]
    fn connected_cells_emit_crossing_glyph() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 0,
                len: 5,
            },
            Divider {
                orientation: DividerOrientation::Horizontal,
                x: 0,
                y: 2,
                len: 5,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let crossing = cells
            .iter()
            .find(|((x, y), _)| *x == 2 && *y == 2)
            .map(|(_, ch)| *ch);

        assert_eq!(crossing, Some('┼'));
    }

    #[test]
    fn connected_cells_bridge_vertical_endpoint_into_horizontal_line() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 3,
                y: 0,
                len: 3,
            },
            Divider {
                orientation: DividerOrientation::Horizontal,
                x: 0,
                y: 3,
                len: 7,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let crossing = cells
            .iter()
            .find(|((x, y), _)| *x == 3 && *y == 3)
            .map(|(_, ch)| *ch);

        assert_eq!(crossing, Some('┴'));
    }

    #[test]
    fn connected_cells_bridge_horizontal_endpoint_into_vertical_line() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Horizontal,
                x: 0,
                y: 3,
                len: 3,
            },
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 3,
                y: 0,
                len: 7,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let crossing = cells
            .iter()
            .find(|((x, y), _)| *x == 3 && *y == 3)
            .map(|(_, ch)| *ch);

        assert_eq!(crossing, Some('┤'));
    }

    #[test]
    fn connected_cells_bridge_single_cell_vertical_gap() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 0,
                len: 2,
            },
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 3,
                len: 2,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let connector = cells
            .iter()
            .find(|((x, y), _)| *x == 2 && *y == 2)
            .map(|(_, ch)| *ch);

        assert_eq!(connector, Some('│'));
    }

    #[test]
    fn connected_cells_bridge_single_cell_vertical_gap_through_horizontal() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 0,
                len: 2,
            },
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 3,
                len: 2,
            },
            Divider {
                orientation: DividerOrientation::Horizontal,
                x: 0,
                y: 2,
                len: 5,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let connector = cells
            .iter()
            .find(|((x, y), _)| *x == 2 && *y == 2)
            .map(|(_, ch)| *ch);

        assert_eq!(connector, Some('┼'));
    }

    #[test]
    fn connected_cells_bridge_two_cell_vertical_gap() {
        let dividers = vec![
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 0,
                len: 2,
            },
            Divider {
                orientation: DividerOrientation::Vertical,
                x: 2,
                y: 4,
                len: 2,
            },
        ];

        let cells = connected_divider_cells(&dividers, 10, 10);
        let first_connector = cells
            .iter()
            .find(|((x, y), _)| *x == 2 && *y == 2)
            .map(|(_, ch)| *ch);
        let second_connector = cells
            .iter()
            .find(|((x, y), _)| *x == 2 && *y == 3)
            .map(|(_, ch)| *ch);

        assert_eq!(first_connector, Some('│'));
        assert_eq!(second_connector, Some('│'));
    }

    #[test]
    fn compose_frame_applies_status_style_to_full_status_row() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: None,
            cursor_style: SetCursorStyle::DefaultUserShape,
        };
        let status_style = CellStyle {
            fg: Some(Color::Rgb {
                r: 0xD8,
                g: 0xDE,
                b: 0xE9,
            }),
            bg: Some(Color::Rgb {
                r: 0x2E,
                g: 0x34,
                b: 0x40,
            }),
            ..CellStyle::default()
        };

        let composed = compose_frame(&frame, "abc", status_style, 6, 2, None, None);
        let status_row = composed.row_slice(1);
        assert_eq!(status_row[0].ch, 'a');
        assert_eq!(status_row[1].ch, 'b');
        assert_eq!(status_row[2].ch, 'c');
        assert_eq!(status_row[3].ch, ' ');
        assert!(status_row.iter().all(|cell| cell.style == status_style));
    }

    #[test]
    fn compose_frame_draws_side_window_tree_with_gt_marker_and_reverse_style() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: None,
            cursor_style: SetCursorStyle::DefaultUserShape,
        };
        let side = SideWindowTree {
            title: "windows".to_string(),
            entries: vec!["w1".to_string(), "w2".to_string()],
            selected: 1,
            width: 8,
        };

        let composed = compose_frame(
            &frame,
            "status",
            CellStyle::default(),
            16,
            5,
            None,
            Some(&side),
        );

        let selected_row = composed.row_slice(2);
        assert_eq!(selected_row[0].ch, '>');
        assert!(selected_row[0].style.reverse);
        assert!(selected_row[1].style.reverse);
        assert_eq!(composed.row_slice(0)[7].ch, '│');
    }

    #[test]
    fn fixed_width_cells_avoids_splitting_wide_char_at_boundary() {
        let cells = vec![
            plain('a'),
            plain('b'),
            plain('c'),
            plain('界'),
            StyledCell {
                ch: '\0',
                ..StyledCell::default()
            },
        ];

        let clipped = fixed_width_cells(&cells, 4);
        assert_eq!(clipped.len(), 4);
        assert_eq!(clipped[0].ch, 'a');
        assert_eq!(clipped[1].ch, 'b');
        assert_eq!(clipped[2].ch, 'c');
        assert_eq!(clipped[3].ch, ' ');
    }

    #[test]
    fn diff_run_resets_style_before_default_chunk() {
        let styled = CellStyle {
            fg: Some(Color::AnsiValue(4)),
            bg: Some(Color::AnsiValue(7)),
            ..CellStyle::default()
        };
        let mut renderer = FrameRenderer::new();
        let first = frame_with_rows(
            5,
            vec![vec![
                plain('a'),
                plain('a'),
                plain('a'),
                plain('a'),
                plain('a'),
            ]],
        );
        let second = frame_with_rows(
            5,
            vec![vec![
                styled_cell('X', styled),
                plain('a'),
                plain('a'),
                plain('Z'),
                plain('a'),
            ]],
        );

        let mut initial_out = Vec::new();
        renderer
            .render_to_writer(&mut initial_out, &first, "s", 5, 2, false, None, None)
            .expect("initial render");

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &second, "s", 5, 2, false, None, None)
            .expect("diff render");
        let ansi = String::from_utf8_lossy(&out);

        assert!(
            ansi.contains("\x1b[0maaZa"),
            "default-style run should include a reset before plain tail cells; output={ansi:?}"
        );
    }

    #[test]
    fn diff_rewrites_row_tail_from_first_changed_cell() {
        let mut renderer = FrameRenderer::new();
        let first = frame_with_rows(5, vec![plain_cells("abcde")]);
        let second = frame_with_rows(5, vec![plain_cells("aXcYe")]);

        let mut initial_out = Vec::new();
        renderer
            .render_to_writer(&mut initial_out, &first, "s", 5, 2, false, None, None)
            .expect("initial render");

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &second, "s", 5, 2, false, None, None)
            .expect("diff render");
        let ansi = String::from_utf8_lossy(&out);

        assert!(
            ansi.contains("XcYe"),
            "expected row tail rewrite from first changed cell; output={ansi:?}"
        );
    }

    #[test]
    fn full_render_resets_style_when_moving_to_next_row() {
        let styled = CellStyle {
            fg: Some(Color::AnsiValue(2)),
            bold: true,
            ..CellStyle::default()
        };
        let mut renderer = FrameRenderer::new();
        let frame = frame_with_rows(
            5,
            vec![
                vec![
                    styled_cell('A', styled),
                    styled_cell('A', styled),
                    styled_cell('A', styled),
                    styled_cell('A', styled),
                    styled_cell('A', styled),
                ],
                vec![plain('b'), plain('b'), plain('b'), plain('b'), plain('b')],
            ],
        );

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &frame, "s", 5, 3, true, None, None)
            .expect("full render");
        let ansi = String::from_utf8_lossy(&out);

        assert!(
            ansi.contains("\x1b[2;1H\x1b[0m"),
            "second row should begin from reset/default style; output={ansi:?}"
        );
    }

    #[test]
    fn compose_frame_styles_adjacent_dividers_for_focused_pane_when_split() {
        let frame = RenderFrame {
            panes: vec![
                RenderPane {
                    pane_id: 1,
                    rect: PaneRect {
                        x: 0,
                        y: 0,
                        width: 4,
                        height: 3,
                    },
                    view_row_origin: 0,
                    rows: vec![
                        plain_cells("aaaa"),
                        plain_cells("bbbb"),
                        plain_cells("cccc"),
                    ],
                    cursor: (0, 0),
                    focused: true,
                },
                RenderPane {
                    pane_id: 2,
                    rect: PaneRect {
                        x: 5,
                        y: 0,
                        width: 4,
                        height: 3,
                    },
                    view_row_origin: 0,
                    rows: vec![
                        plain_cells("1111"),
                        plain_cells("2222"),
                        plain_cells("3333"),
                    ],
                    cursor: (0, 0),
                    focused: false,
                },
            ],
            dividers: vec![
                Divider {
                    orientation: DividerOrientation::Vertical,
                    x: 4,
                    y: 0,
                    len: 5,
                },
                Divider {
                    orientation: DividerOrientation::Horizontal,
                    x: 0,
                    y: 3,
                    len: 8,
                },
            ],
            focused_cursor: Some((0, 0)),
            cursor_style: SetCursorStyle::DefaultUserShape,
        };

        let composed = compose_frame(&frame, "status", CellStyle::default(), 10, 5, None, None);
        let focused_color = focused_pane_border_color();

        // Right-side vertical divider adjacent to focused pane is colored where overlapping.
        for y in 0..3 {
            assert_eq!(composed.row_slice(y)[4].style.fg, Some(focused_color));
            assert_eq!(composed.row_slice(y)[4].style.bg, None);
        }

        // Bottom horizontal divider adjacent to focused pane is colored only under focused width.
        for x in 0..4 {
            assert_eq!(composed.row_slice(3)[x].style.fg, Some(focused_color));
            assert_eq!(composed.row_slice(3)[x].style.bg, None);
        }

        // Unrelated divider segments and pane content are not restyled.
        assert_eq!(composed.row_slice(4)[4].style, CellStyle::default());
        assert_eq!(composed.row_slice(3)[6].style, CellStyle::default());
        assert_eq!(composed.row_slice(1)[1].style, CellStyle::default());
        assert_eq!(composed.row_slice(1)[6].style, CellStyle::default());
    }

    #[test]
    fn compose_frame_does_not_style_border_for_single_pane() {
        let frame = RenderFrame {
            panes: vec![RenderPane {
                pane_id: 1,
                rect: PaneRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 3,
                },
                view_row_origin: 0,
                rows: vec![
                    plain_cells("aaaa"),
                    plain_cells("bbbb"),
                    plain_cells("cccc"),
                ],
                cursor: (0, 0),
                focused: true,
            }],
            dividers: Vec::new(),
            focused_cursor: Some((0, 0)),
            cursor_style: SetCursorStyle::DefaultUserShape,
        };

        let composed = compose_frame(&frame, "status", CellStyle::default(), 10, 5, None, None);
        let focused_color = focused_pane_border_color();

        for y in 0..3 {
            for x in 0..4 {
                assert_ne!(composed.row_slice(y)[x].style.fg, Some(focused_color));
            }
        }
    }

    #[test]
    fn write_styled_cells_emits_style_for_first_default_cell() {
        let mut out = Vec::new();
        let colored = StyledCell {
            ch: 'a',
            style: CellStyle {
                fg: Some(Color::Cyan),
                ..CellStyle::default()
            },
        };
        let plain = StyledCell {
            ch: 'b',
            style: CellStyle::default(),
        };

        write_styled_cells(&mut out, &[colored], 0).expect("write colored cell");
        write_styled_cells(&mut out, &[plain], 0).expect("write plain cell");

        let rendered = String::from_utf8(out).expect("valid utf-8");
        let reset_count = rendered.matches("\u{1b}[0m").count();
        assert!(
            reset_count >= 2,
            "expected at least two reset sequences when style switches from colored to default, got {reset_count}: {rendered:?}"
        );
    }

    #[test]
    fn write_styled_cells_emits_osc8_for_web_urls() {
        let row = plain_cells("see https://example.com/docs now");
        let mut out = Vec::new();
        write_styled_cells(&mut out, &row, 0).expect("write row");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(
            rendered.contains(
                "\u{1b}]8;;https://example.com/docs\u{1b}\\https://example.com/docs\u{1b}]8;;\u{1b}\\"
            ),
            "expected hyperlink sequence in output, got: {rendered:?}"
        );
    }

    #[test]
    fn write_styled_cells_detects_url_when_starting_mid_row() {
        let row = plain_cells(">>> https://example.com/path");
        let mut out = Vec::new();
        write_styled_cells(&mut out, &row, 5).expect("write tail");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(
            rendered.contains(
                "\u{1b}]8;;https://example.com/path\u{1b}\\ttps://example.com/path\u{1b}]8;;\u{1b}\\"
            ),
            "expected hyperlink sequence when rendering from URL start, got: {rendered:?}"
        );
    }

    #[test]
    fn incremental_diff_rewrites_full_url_when_target_changes() {
        let cols = 32;
        let rows = 3;
        let mut renderer = FrameRenderer::new();

        let before = frame_with_rows(cols as usize, vec![plain_cells("https://example.com/a")]);
        let after = frame_with_rows(cols as usize, vec![plain_cells("https://example.com/b")]);

        let mut full_out = Vec::new();
        renderer
            .render_to_writer(
                &mut full_out,
                &before,
                "status",
                cols,
                rows,
                true,
                None,
                None,
            )
            .expect("full render");

        let mut incremental_out = Vec::new();
        renderer
            .render_to_writer(
                &mut incremental_out,
                &after,
                "status",
                cols,
                rows,
                false,
                None,
                None,
            )
            .expect("incremental render");

        let rendered = String::from_utf8(incremental_out).expect("utf8");
        assert!(
            rendered.contains(
                "\u{1b}]8;;https://example.com/b\u{1b}\\https://example.com/b\u{1b}]8;;\u{1b}\\"
            ),
            "expected full rewritten hyperlink target in incremental output, got: {rendered:?}"
        );
    }

    fn frame_with_rows(cols: usize, rows: Vec<Vec<StyledCell>>) -> RenderFrame {
        RenderFrame {
            panes: vec![RenderPane {
                pane_id: 1,
                rect: PaneRect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.len(),
                },
                view_row_origin: 0,
                rows,
                cursor: (0, 0),
                focused: true,
            }],
            dividers: Vec::new(),
            focused_cursor: Some((0, 0)),
            cursor_style: SetCursorStyle::DefaultUserShape,
        }
    }

    fn plain(ch: char) -> StyledCell {
        StyledCell {
            ch,
            ..StyledCell::default()
        }
    }

    fn styled_cell(ch: char, style: CellStyle) -> StyledCell {
        StyledCell { ch, style }
    }

    fn plain_cells(text: &str) -> Vec<StyledCell> {
        text.chars()
            .map(|ch| StyledCell {
                ch,
                ..StyledCell::default()
            })
            .collect()
    }
}
