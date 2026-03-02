use spectra::session::manager::{RenderFrame, RenderPane};
use spectra::session::terminal_state::StyledCell;
use spectra::ui::render::SystemOverlay;
use spectra::ui::window_manager::PaneRect;

mod support;

const COLS: u16 = 70;
const ROWS: u16 = 14;

#[test]
fn centered_system_tree_overlay_snapshot() {
    let frame = sample_frame();
    let overlay = sample_overlay(1);

    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(
        &mut out,
        &frame,
        "tree popup",
        COLS,
        ROWS,
        true,
        Some(&overlay),
        None,
    )
    .expect("render overlay");

    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    support::render_snapshot::assert_rows_match_fixture("system_tree_overlay", &rows);
}

#[test]
fn overlay_cursor_tracks_selected_row() {
    let frame = sample_frame();
    let overlay = sample_overlay(2);

    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(
        &mut out,
        &frame,
        "tree popup",
        COLS,
        ROWS,
        true,
        Some(&overlay),
        None,
    )
    .expect("render overlay");

    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    let selected_row = rows
        .iter()
        .position(|row| row.contains("pane p1 *"))
        .expect("selected line row");

    let cursor = last_cursor_position(&out).expect("cursor position");
    assert_eq!(cursor.0, selected_row);
}

#[test]
fn overlay_cursor_tracks_query_cursor_position() {
    let frame = sample_frame();
    let mut overlay_start = sample_overlay(1);
    overlay_start.query = "abcdef".to_string();
    overlay_start.query_cursor_pos = 0;
    overlay_start.query_active = true;

    let mut out_start = Vec::new();
    spectra::ui::render::render_to_writer(
        &mut out_start,
        &frame,
        "tree popup",
        COLS,
        ROWS,
        true,
        Some(&overlay_start),
        None,
    )
    .expect("render overlay");
    let cursor_start = last_cursor_position(&out_start).expect("cursor position at start");

    let mut overlay_moved = overlay_start;
    overlay_moved.query_cursor_pos = 2;

    let mut out_moved = Vec::new();
    spectra::ui::render::render_to_writer(
        &mut out_moved,
        &frame,
        "tree popup",
        COLS,
        ROWS,
        true,
        Some(&overlay_moved),
        None,
    )
    .expect("render overlay with moved cursor");
    let cursor_moved = last_cursor_position(&out_moved).expect("cursor position after move");

    assert_eq!(cursor_moved.0, cursor_start.0);
    assert_eq!(cursor_moved.1, cursor_start.1 + 2);
}

#[test]
fn overlay_preview_can_render_from_tail() {
    let frame = sample_frame();
    let mut overlay = sample_overlay(1);
    overlay.preview_from_tail = true;
    overlay.preview_lines = (0..40).map(|index| format!("line-{index:03}")).collect();

    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(
        &mut out,
        &frame,
        "tree popup",
        COLS,
        ROWS,
        true,
        Some(&overlay),
        None,
    )
    .expect("render overlay");

    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    assert!(
        rows.iter().any(|row| row.contains("line-039")),
        "expected preview to include newest line from tail"
    );
    assert!(
        !rows.iter().any(|row| row.contains("line-000")),
        "expected preview to skip earliest line when tail mode is enabled"
    );
}

fn sample_frame() -> RenderFrame {
    RenderFrame {
        panes: vec![RenderPane {
            pane_id: 1,
            rect: PaneRect {
                x: 0,
                y: 0,
                width: COLS as usize,
                height: ROWS.saturating_sub(1) as usize,
            },
            view_row_origin: 0,
            rows: vec![plain_cells("$ ready"); ROWS.saturating_sub(1) as usize],
            cursor: (2, 0),
            focused: true,
        }],
        dividers: vec![],
        focused_cursor: Some((2, 0)),
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
    }
}

fn sample_overlay(selected: usize) -> SystemOverlay {
    SystemOverlay {
        title: "tree".to_string(),
        query: String::new(),
        query_cursor_pos: 0,
        query_active: false,
        candidates: vec![
            "- session s1:main *".to_string(),
            "  - window w1 -> p1 *".to_string(),
            "      pane p1 *".to_string(),
        ],
        selected,
        selected_cursor_pos: None,
        preview_lines: vec![
            "type: pane".to_string(),
            "id: p1".to_string(),
            "name: logs".to_string(),
        ],
        preview_from_tail: false,
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

fn last_cursor_position(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    let mut last = None;

    while i + 2 < bytes.len() {
        if bytes[i] == b'\x1b' && bytes[i + 1] == b'[' {
            let mut idx = i + 2;
            while idx < bytes.len() {
                let byte = bytes[idx];
                if (0x40..=0x7e).contains(&byte) {
                    if byte == b'H' {
                        let params = std::str::from_utf8(&bytes[i + 2..idx]).ok()?;
                        let mut parts = params.split(';');
                        let row = parts.next()?.parse::<usize>().ok()?.saturating_sub(1);
                        let col = parts.next()?.parse::<usize>().ok()?.saturating_sub(1);
                        last = Some((row, col));
                    }
                    i = idx;
                    break;
                }
                idx += 1;
            }
        }
        i += 1;
    }

    last
}
