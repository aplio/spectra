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
use crate::ui::style::{apply_style, reset_style};
use crate::ui::text::{display_width, truncate_to_width};
use crate::ui::url::{UrlSpan, find_web_url_spans, write_hyperlink_close, write_hyperlink_open};
use crate::ui::window_manager::{Divider, DividerOrientation};

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

/// Colored agent-state marker drawn in the sidebar (dot or check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentIndicator {
    pub ch: char,
    pub color: Color,
}

impl AgentIndicator {
    /// Sidebar marker for a derived agent display state:
    /// blocked = red dot, working = yellow dot, done = cyan dot,
    /// idle = green check. Unknown carries no marker.
    pub fn for_state(state: crate::agent::AgentDisplayState) -> Option<Self> {
        use crate::agent::AgentDisplayState as S;
        let (ch, color) = match state {
            S::Blocked => ('●', Color::Red),
            S::Working => ('●', Color::Yellow),
            S::Done => ('●', Color::Cyan),
            S::Idle => ('✓', Color::Green),
            S::Unknown => return None,
        };
        Some(Self { ch, color })
    }
}

/// One entry of the sidebar: either a session header or a window under it.
/// An entry occupies one visual row per line; the selection highlight covers
/// all its lines, while the `>` marker and agent indicator sit on the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideTreeEntry {
    pub lines: Vec<String>,
    /// Aggregated agent marker when any pane in the window has an agent.
    pub indicator: Option<AgentIndicator>,
    /// True for a session-name header row; such rows are never selectable and
    /// are drawn without the `>` marker so they group the windows beneath them.
    pub is_header: bool,
}

#[derive(Debug, Clone)]
pub struct SideWindowTree {
    pub title: String,
    pub entries: Vec<SideTreeEntry>,
    pub selected: usize,
    pub width: usize,
}

impl SideWindowTree {
    /// Screen region this sidebar occupies. The left edge is currently the
    /// only supported placement; every geometry consumer must derive
    /// positions from this rect rather than assuming an origin.
    pub fn rect(&self) -> SidebarRect {
        SidebarRect::left_edge(self.width)
    }

    /// `(entry_index, line_index)` per visual row in display order.
    /// Rendering and click hit-testing must both use this flattening so
    /// multi-line entries cannot drift between the two.
    pub fn visual_rows(&self) -> Vec<(usize, usize)> {
        self.entries
            .iter()
            .enumerate()
            .flat_map(|(entry, item)| (0..item.lines.len().max(1)).map(move |line| (entry, line)))
            .collect()
    }

    /// First visual row to display so the selected entry fits within
    /// `visible` rows; when the entry is taller than the viewport its first
    /// line wins.
    pub fn scroll_start(&self, visual_rows: &[(usize, usize)], visible: usize) -> usize {
        let total = visual_rows.len();
        if total == 0 || visible == 0 || total <= visible {
            return 0;
        }
        let selected = self.selected.min(self.entries.len().saturating_sub(1));
        let first = visual_rows
            .iter()
            .position(|(entry, _)| *entry == selected)
            .unwrap_or(0);
        let last = visual_rows
            .iter()
            .rposition(|(entry, _)| *entry == selected)
            .unwrap_or(first);
        let max_start = total - visible;
        last.saturating_add(1)
            .saturating_sub(visible)
            .min(first)
            .min(max_start)
    }
}

/// Screen region occupied by the sidebar, with an explicit origin shared by
/// composition, input hit-testing and pane offset math. The divider column
/// sits on the inner edge, adjacent to the panes. A future placement option
/// only needs to construct a different rect (and flip the divider edge);
/// nothing else may hardcode sidebar coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarRect {
    /// Leftmost column of the sidebar region.
    pub x: usize,
    /// Total width in columns, including the divider column.
    pub width: usize,
}

impl SidebarRect {
    /// Anchor the sidebar at the left screen edge (the only configuration
    /// currently in use).
    pub fn left_edge(width: usize) -> Self {
        Self { x: 0, width }
    }

    /// Same region clamped to `total_cols` available screen columns.
    pub fn clamped_to(self, total_cols: usize) -> Self {
        Self {
            x: self.x.min(total_cols),
            width: self.width.min(total_cols.saturating_sub(self.x)),
        }
    }

    /// First column of the header/entry content area.
    pub fn content_x(&self) -> usize {
        self.x
    }

    /// Columns available for content, excluding the divider column.
    pub fn content_width(&self) -> usize {
        self.width.saturating_sub(1)
    }

    /// Column of the divider separating the sidebar from the panes.
    pub fn divider_x(&self) -> usize {
        self.x + self.content_width()
    }

    /// Whether `col` lands inside the content area (divider excluded).
    pub fn contains_content_col(&self, col: usize) -> bool {
        (self.content_x()..self.content_x() + self.content_width()).contains(&col)
    }

    /// Columns panes must shift right by to clear the sidebar region.
    pub fn pane_x_offset(&self) -> usize {
        self.x + self.width
    }
}

pub struct FrameRenderer {
    previous: Option<BackBuffer>,
    /// Cursor color last forwarded to the host terminal (guest OSC 12).
    /// `None` after an OSC 112 reset or when never set.
    cursor_color: Option<(u8, u8, u8)>,
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
    cursor_visible: bool,
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
            cursor_visible: true,
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
        Self {
            previous: None,
            cursor_color: None,
        }
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
        // Follow the focused pane's OSC 12 cursor color on the host,
        // restoring the host default (OSC 112) when the pane clears it or
        // focus moves to a pane without one.
        if frame.cursor_color != self.cursor_color {
            let sequence = match frame.cursor_color {
                Some(rgb) => crate::io::terminal::osc12_cursor_color_sequence(rgb),
                None => crate::io::terminal::osc112_reset_cursor_color_sequence(),
            };
            writer.write_all(sequence.as_bytes())?;
            self.cursor_color = frame.cursor_color;
        }
        // The host cursor is parked at the frame cursor cell even when it
        // stays hidden, so IMEs that anchor their candidate window to the
        // real cursor keep pointing at the focused pane's cursor.
        queue!(writer, MoveTo(composed.cursor.0, composed.cursor.1))?;
        if composed.cursor_visible {
            queue!(writer, composed.cursor_style, cursor::Show)?;
        }
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
    // Overlay text inputs are spectra's own cursor and always show; otherwise
    // the guest's DECTCEM state for the focused pane decides.
    composed.cursor_visible = overlay_cursor.is_some() || !frame.focused_cursor_hidden;

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
    let rect = side.rect().clamped_to(total_cols.saturating_sub(1));
    if rect.width < 4 {
        return;
    }

    let divider_x = rect.divider_x();
    let content_x = rect.content_x();
    let content_w = rect.content_width();
    let header = fixed_width(&side.title, content_w);
    draw_text_with_style(
        frame,
        content_x,
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
            link: None,
        },
    );

    let content_h = workspace_rows.saturating_sub(1);
    if content_h == 0 {
        return;
    }

    let selected = side.selected.min(side.entries.len().saturating_sub(1));
    let visual_rows = side.visual_rows();
    let start = side.scroll_start(&visual_rows, content_h);
    for row in 0..content_h {
        let y = 1 + row;
        let Some(&(entry_idx, line_idx)) = visual_rows.get(start + row) else {
            draw_text(frame, content_x, y, &fixed_width("", content_w));
            continue;
        };
        let entry = &side.entries[entry_idx];
        let label = entry.lines.get(line_idx).map(String::as_str).unwrap_or("");

        // Session headers group the windows beneath them: no marker, drawn
        // bold so they stand apart from the (indented) window rows.
        if entry.is_header {
            draw_text_with_style(
                frame,
                content_x,
                y,
                &fixed_width(label, content_w),
                CellStyle {
                    bold: true,
                    ..CellStyle::default()
                },
            );
            continue;
        }

        let is_selected = entry_idx == selected && !side.entries.is_empty();
        // The `>` marker and agent indicator sit on the entry's first line;
        // continuation lines keep the marker column blank so they stay
        // aligned, and the reverse highlight spans every line.
        let marker = if is_selected && line_idx == 0 {
            '>'
        } else {
            ' '
        };
        let indicator = if line_idx == 0 { entry.indicator } else { None };
        // With an agent indicator, the last two content columns are reserved
        // for ` ●` so the marker never overflows into the divider.
        let line = match indicator {
            Some(indicator) => {
                let mut line =
                    fixed_width(&format!("{marker} {label}"), content_w.saturating_sub(2));
                line.push(' ');
                line.push(indicator.ch);
                line
            }
            None => fixed_width(&format!("{marker} {label}"), content_w),
        };
        draw_text_with_style(
            frame,
            content_x,
            y,
            &line,
            if is_selected {
                CellStyle {
                    reverse: true,
                    ..CellStyle::default()
                }
            } else {
                CellStyle::default()
            },
        );
        if let Some(indicator) = indicator {
            frame.set_fg(content_x + content_w.saturating_sub(1), y, indicator.color);
        }
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
        frame.set(
            x + col,
            y,
            StyledCell {
                ch,
                style,
                link: None,
            },
        );
        if w == 2 {
            frame.set(
                x + col + 1,
                y,
                StyledCell {
                    ch: '\0',
                    style,
                    link: None,
                },
            );
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
        let cell = cells[index].clone();
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
            let Some(continuation) = cells.get(index + 1).cloned() else {
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
        // Explicit OSC 8 links from the guest take priority over URLs
        // auto-detected from the row text.
        let url_for_cell = cell.link.as_deref().or_else(|| row_urls.url_for_col(idx));
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
        AgentIndicator, DOWN, FrameRenderer, LEFT, RIGHT, SideTreeEntry, SideWindowTree,
        SystemOverlay, UP, compose_frame, connected_divider_cells, divider_glyph,
        fixed_width_cells, focused_pane_border_color, render_to_writer, write_styled_cells,
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
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
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
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };
        let side = SideWindowTree {
            title: "windows".to_string(),
            entries: vec![
                SideTreeEntry {
                    lines: vec!["w1".to_string()],
                    indicator: None,
                    is_header: false,
                },
                SideTreeEntry {
                    lines: vec!["w2".to_string()],
                    indicator: None,
                    is_header: false,
                },
            ],
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
    fn compose_frame_draws_multi_line_sidebar_entry_with_full_highlight() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: None,
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };
        let side = SideWindowTree {
            title: "windows".to_string(),
            entries: vec![
                SideTreeEntry {
                    lines: vec!["w1".to_string(), " extra".to_string()],
                    indicator: None,
                    is_header: false,
                },
                SideTreeEntry {
                    lines: vec!["w2".to_string()],
                    indicator: None,
                    is_header: false,
                },
            ],
            selected: 0,
            width: 8,
        };

        let composed = compose_frame(
            &frame,
            "status",
            CellStyle::default(),
            16,
            6,
            None,
            Some(&side),
        );

        // First line carries the `>` marker; the continuation line keeps the
        // marker column blank but shares the reverse highlight.
        let first = composed.row_slice(1);
        assert_eq!(first[0].ch, '>');
        assert!(first[0].style.reverse);
        let second = composed.row_slice(2);
        assert_eq!(second[0].ch, ' ');
        assert_eq!(second[3].ch, 'e');
        assert!(second[0].style.reverse);
        assert!(second[3].style.reverse);
        // The next entry starts on the following row, unhighlighted.
        let third = composed.row_slice(3);
        assert_eq!(third[2].ch, 'w');
        assert!(!third[2].style.reverse);
    }

    #[test]
    fn side_window_tree_scroll_keeps_multi_line_selection_fully_visible() {
        let window = |lines: Vec<&str>| SideTreeEntry {
            lines: lines.into_iter().map(str::to_string).collect(),
            indicator: None,
            is_header: false,
        };
        let side = SideWindowTree {
            title: "windows".to_string(),
            entries: vec![
                SideTreeEntry {
                    lines: vec!["s".to_string()],
                    indicator: None,
                    is_header: true,
                },
                window(vec!["w1", "a"]),
                window(vec!["w2", "b"]),
                window(vec!["w3", "c"]),
            ],
            selected: 3,
            width: 8,
        };

        let visual = side.visual_rows();
        assert_eq!(visual.len(), 7);
        // With 3 visible rows, both lines of the selected last entry stay in
        // view (start row 4 shows w2's tail plus w3 in full).
        assert_eq!(side.scroll_start(&visual, 3), 4);
        // Everything fits: no scrolling.
        assert_eq!(side.scroll_start(&visual, 7), 0);
    }

    #[test]
    fn agent_indicator_maps_states_to_marker_and_color() {
        use crate::agent::AgentDisplayState as S;

        let blocked = AgentIndicator::for_state(S::Blocked).expect("blocked marker");
        assert_eq!((blocked.ch, blocked.color), ('●', Color::Red));
        let working = AgentIndicator::for_state(S::Working).expect("working marker");
        assert_eq!((working.ch, working.color), ('●', Color::Yellow));
        let done = AgentIndicator::for_state(S::Done).expect("done marker");
        assert_eq!((done.ch, done.color), ('●', Color::Cyan));
        let idle = AgentIndicator::for_state(S::Idle).expect("idle marker");
        assert_eq!((idle.ch, idle.color), ('✓', Color::Green));
        assert_eq!(AgentIndicator::for_state(S::Unknown), None);
    }

    #[test]
    fn compose_frame_draws_side_window_tree_agent_marker_inside_divider() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: None,
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };
        let side = SideWindowTree {
            title: "windows".to_string(),
            entries: vec![
                SideTreeEntry {
                    lines: vec!["w1:very-long-window-name".to_string()],
                    indicator: AgentIndicator::for_state(crate::agent::AgentDisplayState::Blocked),
                    is_header: false,
                },
                SideTreeEntry {
                    lines: vec!["w2".to_string()],
                    indicator: None,
                    is_header: false,
                },
            ],
            selected: 0,
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

        // Width 8 leaves content columns 0..=6 and the divider at column 7.
        // The blocked marker occupies the last content column, red, with a
        // separating space, and the long label is truncated before it.
        let marked_row = composed.row_slice(1);
        assert_eq!(marked_row[6].ch, '●');
        assert_eq!(marked_row[6].style.fg, Some(Color::Red));
        assert_eq!(marked_row[5].ch, ' ');
        assert_eq!(marked_row[7].ch, '│');
        // A row without an agent keeps its full label width and no marker.
        let plain_row = composed.row_slice(2);
        assert_eq!(plain_row[6].ch, ' ');
        assert_eq!(plain_row[6].style.fg, None);
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
    fn sidebar_rect_left_edge_geometry() {
        let rect = super::SidebarRect::left_edge(8);
        assert_eq!(rect.content_x(), 0);
        assert_eq!(rect.content_width(), 7);
        assert_eq!(rect.divider_x(), 7);
        assert_eq!(rect.pane_x_offset(), 8);
        // Content spans [0, 7): both edges hit-test correctly.
        assert!(rect.contains_content_col(0));
        assert!(rect.contains_content_col(6));
        assert!(!rect.contains_content_col(7), "divider is not content");
        assert!(!rect.contains_content_col(8));
    }

    #[test]
    fn sidebar_rect_geometry_follows_nonzero_origin() {
        // Position is a parameter: nothing may assume the left screen edge.
        let rect = super::SidebarRect { x: 5, width: 8 };
        assert_eq!(rect.content_x(), 5);
        assert_eq!(rect.content_width(), 7);
        assert_eq!(rect.divider_x(), 12);
        assert_eq!(rect.pane_x_offset(), 13);
        assert!(!rect.contains_content_col(4), "before the sidebar");
        assert!(rect.contains_content_col(5), "first content column");
        assert!(rect.contains_content_col(11), "last content column");
        assert!(!rect.contains_content_col(12), "divider is not content");
        assert!(!rect.contains_content_col(13));
    }

    #[test]
    fn sidebar_rect_clamps_to_available_columns() {
        let rect = super::SidebarRect::left_edge(28).clamped_to(10);
        assert_eq!(rect, super::SidebarRect { x: 0, width: 10 });
        let offset = super::SidebarRect { x: 8, width: 8 }.clamped_to(10);
        assert_eq!(offset, super::SidebarRect { x: 8, width: 2 });
        let past_end = super::SidebarRect { x: 12, width: 8 }.clamped_to(10);
        assert_eq!(past_end, super::SidebarRect { x: 10, width: 0 });
    }

    #[test]
    fn claude_code_style_underline_off_reaches_renderer_output() {
        // End-to-end regression for stray underlines: a guest draws a curly
        // underline (SGR 4:3), turns it off colon-style (SGR 4:0), then
        // prints a plain prompt line. The renderer must emit the plain row
        // from a reset (underline-off) state and never re-underline it.
        let mut state = crate::session::terminal_state::TerminalState::new(16, 2);
        state.feed(b"\x1b[4:3munderlined\x1b[4:0m\r\nplain>");

        let mut renderer = FrameRenderer::new();
        let frame = frame_with_rows(16, vec![state.row_cells(0), state.row_cells(1)]);
        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &frame, "s", 16, 3, true, None, None)
            .expect("full render");
        let ansi = String::from_utf8_lossy(&out);

        let (_, after_second_row) = ansi
            .split_once("\x1b[2;1H")
            .expect("second row is rendered");
        assert!(
            after_second_row.contains("\x1b[0m") && after_second_row.contains("plain>"),
            "plain row must start from a reset style; output={ansi:?}"
        );
        assert!(
            !after_second_row.contains("\x1b[4m"),
            "plain row must not re-enable underline; output={ansi:?}"
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
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
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
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
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
    fn hidden_focused_cursor_keeps_host_cursor_hidden_but_parked() {
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
                rows: vec![plain_cells("aaaa")],
                cursor: (2, 0),
                focused: true,
            }],
            dividers: Vec::new(),
            focused_cursor: Some((2, 0)),
            focused_cursor_hidden: true,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };

        let mut out = Vec::new();
        render_to_writer(&mut out, &frame, "status", 10, 5, true, None, None)
            .expect("render hidden cursor frame");
        let text = String::from_utf8_lossy(&out);
        assert!(
            !text.contains("\x1b[?25h"),
            "host cursor must stay hidden when the guest hid it: {text:?}"
        );
        // The cursor is still parked at the pane cursor cell for IMEs that
        // anchor to the real cursor position (final MoveTo is 1-based).
        assert!(
            text.ends_with("\x1b[1;3H"),
            "host cursor should be parked at the hidden cursor cell: {text:?}"
        );
    }

    #[test]
    fn visible_focused_cursor_shows_host_cursor() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: Some((0, 0)),
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };

        let mut out = Vec::new();
        render_to_writer(&mut out, &frame, "status", 10, 5, true, None, None)
            .expect("render visible cursor frame");
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("\x1b[?25h"),
            "host cursor should be shown when the guest cursor is visible: {text:?}"
        );
    }

    #[test]
    fn overlay_cursor_shows_even_when_guest_hid_cursor() {
        let frame = RenderFrame {
            panes: Vec::new(),
            dividers: Vec::new(),
            focused_cursor: Some((0, 0)),
            focused_cursor_hidden: true,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        };
        let overlay = SystemOverlay {
            title: "tree".to_string(),
            query: String::new(),
            query_cursor_pos: 0,
            query_active: true,
            candidates: vec!["one".to_string()],
            selected: 0,
            selected_cursor_pos: None,
            preview_lines: vec!["preview".to_string()],
            preview_from_tail: false,
        };

        let mut out = Vec::new();
        render_to_writer(
            &mut out,
            &frame,
            "status",
            40,
            12,
            true,
            Some(&overlay),
            None,
        )
        .expect("render overlay frame");
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("\x1b[?25h"),
            "overlay text input cursor must show regardless of guest DECTCEM: {text:?}"
        );
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
            link: None,
        };
        let plain = StyledCell {
            ch: 'b',
            style: CellStyle::default(),
            link: None,
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
    fn write_styled_cells_emits_osc8_for_cell_level_links() {
        let mut row = plain_cells("open docs now");
        let link: std::sync::Arc<str> = std::sync::Arc::from("https://example.com");
        for cell in row.iter_mut().take(4) {
            cell.link = Some(link.clone());
        }

        let mut out = Vec::new();
        write_styled_cells(&mut out, &row, 0).expect("write row");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(
            rendered
                .contains("\u{1b}]8;;https://example.com\u{1b}\\open\u{1b}]8;;\u{1b}\\ docs now"),
            "expected OSC 8 wrapping only the linked cells, got: {rendered:?}"
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

    /// Number of OSC 8 opens (non-empty URI) and closes in renderer output.
    fn osc8_open_close_counts(rendered: &str) -> (usize, usize) {
        let total = rendered.matches("\u{1b}]8;;").count();
        let closes = rendered.matches("\u{1b}]8;;\u{1b}\\").count();
        (total - closes, closes)
    }

    #[test]
    fn incremental_repaint_of_linked_row_reopens_and_closes_osc8() {
        let cols = 40;
        let rows = 3;
        let mut renderer = FrameRenderer::new();

        let before = frame_with_rows(
            cols as usize,
            vec![plain_cells("see https://example.com/docs now")],
        );
        let after = frame_with_rows(
            cols as usize,
            vec![plain_cells("SEE https://example.com/docs now")],
        );

        let mut full_out = Vec::new();
        renderer
            .render_to_writer(&mut full_out, &before, "s", cols, rows, true, None, None)
            .expect("full render");

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &after, "s", cols, rows, false, None, None)
            .expect("incremental render");
        let rendered = String::from_utf8(out).expect("utf8");

        // The repainted tail crosses the linked region: the URL must be
        // re-emitted as a complete OSC 8 open + close pair.
        assert!(
            rendered.contains(
                "\u{1b}]8;;https://example.com/docs\u{1b}\\https://example.com/docs\u{1b}]8;;\u{1b}\\"
            ),
            "expected re-opened and closed hyperlink in incremental output, got: {rendered:?}"
        );
        let (opens, closes) = osc8_open_close_counts(&rendered);
        assert_eq!(
            opens, closes,
            "incremental output must not leave an OSC 8 region open: {rendered:?}"
        );
    }

    #[test]
    fn incremental_repaint_inside_cell_level_link_stays_balanced() {
        let cols = 24;
        let rows = 3;
        let mut renderer = FrameRenderer::new();
        let link: std::sync::Arc<str> = std::sync::Arc::from("https://example.com");

        let linked_row = |text: &str| {
            let mut row = plain_cells(text);
            for cell in row.iter_mut().take(4) {
                cell.link = Some(link.clone());
            }
            row
        };
        let before = frame_with_rows(cols as usize, vec![linked_row("docs and more")]);
        let after = frame_with_rows(cols as usize, vec![linked_row("dXcs and more")]);

        let mut full_out = Vec::new();
        renderer
            .render_to_writer(&mut full_out, &before, "s", cols, rows, true, None, None)
            .expect("full render");

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &after, "s", cols, rows, false, None, None)
            .expect("incremental render");
        let rendered = String::from_utf8(out).expect("utf8");

        // Repaint starts mid-link: the repainted linked cells must be
        // wrapped in a fresh OSC 8 open + close, and the close must land
        // before the unlinked tail.
        assert!(
            rendered
                .contains("\u{1b}]8;;https://example.com\u{1b}\\Xcs\u{1b}]8;;\u{1b}\\ and more"),
            "expected repainted linked cells wrapped in OSC 8, got: {rendered:?}"
        );
        let (opens, closes) = osc8_open_close_counts(&rendered);
        assert_eq!(
            opens, closes,
            "incremental output must not leave an OSC 8 region open: {rendered:?}"
        );
    }

    #[test]
    fn incremental_repaint_after_link_does_not_touch_link_region() {
        let cols = 40;
        let rows = 3;
        let mut renderer = FrameRenderer::new();

        let before = frame_with_rows(
            cols as usize,
            vec![plain_cells("https://example.com/docs now")],
        );
        let after = frame_with_rows(
            cols as usize,
            vec![plain_cells("https://example.com/docs NOW")],
        );

        let mut full_out = Vec::new();
        renderer
            .render_to_writer(&mut full_out, &before, "s", cols, rows, true, None, None)
            .expect("full render");
        let full_rendered = String::from_utf8(full_out).expect("utf8");
        let (full_opens, full_closes) = osc8_open_close_counts(&full_rendered);
        assert!(full_opens > 0, "full frame must emit the link");
        assert_eq!(full_opens, full_closes, "full frame must close its links");

        let mut out = Vec::new();
        renderer
            .render_to_writer(&mut out, &after, "s", cols, rows, false, None, None)
            .expect("incremental render");
        let rendered = String::from_utf8(out).expect("utf8");

        // The change is after the URL, so the repaint may skip the link
        // entirely - but whatever is emitted must stay balanced.
        let (opens, closes) = osc8_open_close_counts(&rendered);
        assert_eq!(
            opens, closes,
            "incremental output must not leave an OSC 8 region open: {rendered:?}"
        );
        assert!(
            rendered.contains("NOW"),
            "changed tail must be repainted: {rendered:?}"
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
            focused_cursor_hidden: false,
            cursor_style: SetCursorStyle::DefaultUserShape,
            cursor_color: None,
        }
    }

    fn plain(ch: char) -> StyledCell {
        StyledCell {
            ch,
            ..StyledCell::default()
        }
    }

    fn styled_cell(ch: char, style: CellStyle) -> StyledCell {
        StyledCell {
            ch,
            style,
            link: None,
        }
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
