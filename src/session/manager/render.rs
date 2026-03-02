use std::collections::HashSet;

use super::*;

impl SessionManager {
    pub fn frame(&self, cols: u16, rows: u16) -> RenderFrame {
        let area = workspace_area(cols, rows);
        let Some(window) = self.active_window() else {
            return empty_frame();
        };
        let focused_id = window.manager.focused_pane_id();
        let layout = window.manager.layout(area);

        let mut panes = Vec::new();
        let mut focused_cursor = None;
        let mut cursor_style = crossterm::cursor::SetCursorStyle::DefaultUserShape;

        for pane_layout in &layout.panes {
            if let Some(pane) = self.panes.get(&pane_layout.pane_id) {
                let cursor = pane.cursor();
                let view_rows = pane_layout.rect.height;
                let view_row_origin = pane.view_row_origin_for(view_rows);
                let lines = pane.row_cells_for_view(view_rows);
                let is_focused = focused_id == Some(pane_layout.pane_id);
                if is_focused {
                    let max_x = pane_layout.rect.width.saturating_sub(1);
                    let max_y = pane_layout.rect.height.saturating_sub(1);
                    if let Some(visible_cursor_y) = pane.cursor_row_in_view(view_rows) {
                        let cursor_x = pane_layout.rect.x + cursor.0.min(max_x);
                        let cursor_y = pane_layout.rect.y + visible_cursor_y.min(max_y);
                        focused_cursor = Some((cursor_x as u16, cursor_y as u16));
                        cursor_style = pane.cursor_style();
                    }
                }
                panes.push(RenderPane {
                    pane_id: pane_layout.pane_id,
                    rect: pane_layout.rect,
                    view_row_origin,
                    rows: lines,
                    cursor,
                    focused: is_focused,
                });
            }
        }

        RenderFrame {
            panes,
            dividers: layout.dividers,
            focused_cursor,
            cursor_style,
        }
    }

    pub fn peek_all_panes_frame(&self, cols: u16, rows: u16) -> RenderFrame {
        let area = workspace_area(cols, rows);
        let pane_ids = self.peek_ordered_pane_ids();
        if pane_ids.is_empty() {
            return empty_frame();
        }

        let (rects, dividers) = equal_grid_layout(area, pane_ids.len());
        let focused_id = self.focused_pane_id();

        let mut panes = Vec::new();
        let mut focused_cursor = None;
        let mut cursor_style = crossterm::cursor::SetCursorStyle::DefaultUserShape;

        for (pane_id, rect) in pane_ids.into_iter().zip(rects.into_iter()) {
            if let Some(pane) = self.panes.get(&pane_id) {
                let cursor = pane.cursor();
                let view_rows = rect.height;
                let view_row_origin = pane.view_row_origin_for(view_rows);
                let lines = pane.row_cells_for_view(view_rows);
                let is_focused = focused_id == Some(pane_id);
                if is_focused {
                    let max_x = rect.width.saturating_sub(1);
                    let max_y = rect.height.saturating_sub(1);
                    if let Some(visible_cursor_y) = pane.cursor_row_in_view(view_rows) {
                        let cursor_x = rect.x + cursor.0.min(max_x);
                        let cursor_y = rect.y + visible_cursor_y.min(max_y);
                        focused_cursor = Some((cursor_x as u16, cursor_y as u16));
                        cursor_style = pane.cursor_style();
                    }
                }
                panes.push(RenderPane {
                    pane_id,
                    rect,
                    view_row_origin,
                    rows: lines,
                    cursor,
                    focused: is_focused,
                });
            }
        }

        RenderFrame {
            panes,
            dividers,
            focused_cursor,
            cursor_style,
        }
    }

    fn peek_ordered_pane_ids(&self) -> Vec<PaneId> {
        let mut pane_ids = Vec::new();
        let mut seen = HashSet::new();

        for window in &self.windows {
            for pane_id in window.manager.ordered_pane_ids() {
                if seen.insert(pane_id) {
                    pane_ids.push(pane_id);
                }
            }

            if let Some(snapshot) = window.zoom_snapshot.as_ref() {
                for pane_id in snapshot.ordered_pane_ids() {
                    if seen.insert(pane_id) {
                        pane_ids.push(pane_id);
                    }
                }
            }
        }

        pane_ids
    }

    pub fn layout_snapshot(&self, cols: u16, rows: u16) -> SavedLayout {
        let area = workspace_area(cols, rows);
        let windows = self
            .windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                let layout = window.manager.layout(area);
                let focused = window.manager.focused_pane_id();
                let panes = layout
                    .panes
                    .iter()
                    .map(|pane| SavedPaneLayout {
                        pane_id: pane.pane_id,
                        rect: pane.rect,
                        focused: focused == Some(pane.pane_id),
                        preview: self
                            .panes
                            .get(&pane.pane_id)
                            .map(|p| p.row_text(0).trim_end().to_string())
                            .unwrap_or_default(),
                    })
                    .collect::<Vec<_>>();
                SavedWindowLayout {
                    index: index + 1,
                    window_id: window.id,
                    focused: index == self.active_window,
                    focused_pane_id: focused,
                    panes,
                    dividers: layout.dividers,
                }
            })
            .collect::<Vec<_>>();

        SavedLayout {
            session_name: self.session_name.clone(),
            focused_window_number: self.focused_window_number(),
            focused_pane_id: self.focused_pane_id(),
            windows,
        }
    }
}

fn empty_frame() -> RenderFrame {
    RenderFrame {
        panes: Vec::new(),
        dividers: Vec::new(),
        focused_cursor: None,
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
    }
}

fn equal_grid_layout(area: PaneRect, pane_count: usize) -> (Vec<PaneRect>, Vec<Divider>) {
    if pane_count == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut columns = 1usize;
    while columns.saturating_mul(columns) < pane_count {
        columns += 1;
    }
    let rows = pane_count.div_ceil(columns);

    let widths = equal_partitions(area.width, columns);
    let heights = equal_partitions(area.height, rows);
    let x_offsets = cumulative_offsets(&widths);
    let y_offsets = cumulative_offsets(&heights);

    let mut panes = Vec::with_capacity(pane_count);
    for index in 0..pane_count {
        let row = index / columns;
        let column = index % columns;
        panes.push(PaneRect {
            x: area.x + x_offsets[column],
            y: area.y + y_offsets[row],
            width: widths[column],
            height: heights[row],
        });
    }

    let mut dividers = Vec::new();
    for x_offset in x_offsets.iter().skip(1) {
        dividers.push(Divider {
            orientation: crate::ui::window_manager::DividerOrientation::Vertical,
            x: area.x + *x_offset,
            y: area.y,
            len: area.height,
        });
    }
    for y_offset in y_offsets.iter().skip(1) {
        dividers.push(Divider {
            orientation: crate::ui::window_manager::DividerOrientation::Horizontal,
            x: area.x,
            y: area.y + *y_offset,
            len: area.width,
        });
    }

    (panes, dividers)
}

fn equal_partitions(total: usize, parts: usize) -> Vec<usize> {
    if parts == 0 {
        return Vec::new();
    }
    let base = total / parts;
    let remainder = total % parts;
    (0..parts)
        .map(|index| base + usize::from(index < remainder))
        .collect()
}

fn cumulative_offsets(parts: &[usize]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(parts.len());
    let mut current = 0usize;
    for size in parts {
        offsets.push(current);
        current += size;
    }
    offsets
}
