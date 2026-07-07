#[allow(dead_code)]
mod support;

use spectra::session::manager::{RenderFrame, RenderPane};
use spectra::session::terminal_state::StyledCell;
use spectra::ui::render::FrameRenderer;
use spectra::ui::window_manager::{Divider, DividerOrientation, PaneRect};

use support::render_snapshot::ansi_bytes_to_rows;

#[test]
fn full_clear_only_on_full_clear_render() {
    let mut renderer = FrameRenderer::new();
    let frame = sample_frame(10, "hello", "world", (0, 0));

    let mut first = Vec::new();
    renderer
        .render_to_writer(&mut first, &frame, "status", 10, 3, true, None, None)
        .expect("render with full clear");
    assert!(
        contains_bytes(&first, b"\x1b[2J"),
        "first render should clear screen"
    );

    let mut second = Vec::new();
    renderer
        .render_to_writer(&mut second, &frame, "status", 10, 3, false, None, None)
        .expect("render without full clear");
    assert!(
        !contains_bytes(&second, b"\x1b[2J"),
        "incremental render should not clear screen"
    );
}

#[test]
fn resized_frame_incremental_render_does_not_clear_screen() {
    let mut renderer = FrameRenderer::new();
    let first_frame = sample_frame(10, "hello", "world", (0, 0));
    let frame = sample_frame(12, "resized", "frame", (0, 0));

    let mut first = Vec::new();
    renderer
        .render_to_writer(&mut first, &first_frame, "status", 10, 3, false, None, None)
        .expect("initial render");

    let mut out = Vec::new();
    renderer
        .render_to_writer(&mut out, &frame, "status", 12, 3, false, None, None)
        .expect("incremental render after resize");
    assert!(
        !contains_bytes(&out, b"\x1b[2J"),
        "resize-path incremental render should not clear screen"
    );
    assert!(
        contains_bytes(&out, b"resized"),
        "dimension changes should trigger a repaint of visible content"
    );
}

#[test]
fn single_cell_update_emits_small_diff_only() {
    let mut renderer = FrameRenderer::new();
    let first_frame = sample_frame(10, "hello", "world", (0, 0));
    let second_frame = sample_frame(10, "hallo", "world", (0, 0));

    let mut first = Vec::new();
    renderer
        .render_to_writer(&mut first, &first_frame, "status", 10, 3, false, None, None)
        .expect("initial render");

    let mut second = Vec::new();
    renderer
        .render_to_writer(
            &mut second,
            &second_frame,
            "status",
            10,
            3,
            false,
            None,
            None,
        )
        .expect("incremental render");

    assert!(
        !contains_bytes(&second, b"\x1b[2J"),
        "diff renders should not clear the screen"
    );
    assert!(
        second.len() < first.len(),
        "diff render should stay smaller than initial full repaint (first={}, second={})",
        first.len(),
        second.len()
    );
    assert!(
        contains_bytes(&second, b"allo"),
        "changed row tail should be emitted in diff output"
    );
    assert!(
        !contains_bytes(&second, b"world"),
        "unchanged rows should not be repainted"
    );
}

#[test]
fn cursor_only_update_moves_cursor_without_repainting_text() {
    let mut renderer = FrameRenderer::new();
    let first_frame = sample_frame(10, "hello", "world", (0, 0));
    let second_frame = sample_frame(10, "hello", "world", (4, 1));

    let mut first = Vec::new();
    renderer
        .render_to_writer(&mut first, &first_frame, "status", 10, 3, false, None, None)
        .expect("initial render");

    let mut second = Vec::new();
    renderer
        .render_to_writer(
            &mut second,
            &second_frame,
            "status",
            10,
            3,
            false,
            None,
            None,
        )
        .expect("cursor-only incremental render");

    assert!(
        !contains_bytes(&second, b"\x1b[2J"),
        "cursor-only update should not clear screen"
    );
    assert!(
        !contains_bytes(&second, b"hello") && !contains_bytes(&second, b"world"),
        "cursor-only update should not repaint unchanged pane text"
    );
    assert!(
        contains_bytes(&second, b"\x1b[2;5H"),
        "cursor-only update should move cursor to the requested cell"
    );
    assert!(
        contains_bytes(&second, b"\x1b[?25l") && contains_bytes(&second, b"\x1b[?25h"),
        "renderer should hide cursor during paint and show it after"
    );
}

#[test]
fn full_clear_flag_forces_clear_even_with_back_buffer() {
    let mut renderer = FrameRenderer::new();
    let frame = sample_frame(10, "hello", "world", (0, 0));

    let mut first = Vec::new();
    renderer
        .render_to_writer(&mut first, &frame, "status", 10, 3, false, None, None)
        .expect("initial render");

    let mut second = Vec::new();
    renderer
        .render_to_writer(&mut second, &frame, "status", 10, 3, true, None, None)
        .expect("forced full clear render");

    assert!(
        contains_bytes(&second, b"\x1b[2J"),
        "full_clear must force a terminal clear even when back buffer exists"
    );
}

fn sample_frame(cols: usize, row0: &str, row1: &str, cursor: (u16, u16)) -> RenderFrame {
    RenderFrame {
        panes: vec![RenderPane {
            pane_id: 1,
            rect: PaneRect {
                x: 0,
                y: 0,
                width: cols,
                height: 2,
            },
            view_row_origin: 0,
            rows: vec![plain_cells(row0), plain_cells(row1)],
            cursor: (0, 0),
            focused: true,
        }],
        dividers: vec![],
        focused_cursor: Some(cursor),
        focused_cursor_hidden: false,
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
        cursor_color: None,
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

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Invariant: replaying any accumulated incremental diff stream onto a blank
/// terminal must produce exactly the screen a fresh full-frame render of the
/// final frame produces. Runs a few scripted frame sequences that exercise
/// shrinking rows, cursor-only moves and multi-pane updates.
#[test]
fn incremental_diff_stream_replays_to_the_final_full_frame() {
    const COLS: usize = 14;
    const ROWS: usize = 4;

    let scenarios: Vec<(&str, Vec<RenderFrame>)> = vec![
        (
            "shrinking row leaves no residue",
            vec![
                tall_frame(COLS, &["hello world!", "second line", "third"], (0, 0)),
                tall_frame(COLS, &["hi", "second line", "third"], (2, 0)),
                tall_frame(COLS, &["hi", "s", ""], (1, 1)),
            ],
        ),
        (
            "cursor-only moves keep content stable",
            vec![
                tall_frame(COLS, &["alpha", "beta", "gamma"], (0, 0)),
                tall_frame(COLS, &["alpha", "beta", "gamma"], (4, 2)),
                tall_frame(COLS, &["alpha", "beta!", "gamma"], (5, 1)),
            ],
        ),
        (
            "two panes with divider update independently",
            vec![
                split_frame(COLS, &["left1", "left2"], &["right1", "right2"], (0, 0)),
                split_frame(COLS, &["LEFT1", "left2"], &["right1", "right2"], (1, 0)),
                split_frame(COLS, &["LEFT1", "left2"], &["r", "right2"], (1, 1)),
            ],
        ),
    ];

    for (name, frames) in scenarios {
        let mut incremental = FrameRenderer::new();
        let mut stream = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            incremental
                .render_to_writer(
                    &mut stream,
                    frame,
                    "status",
                    COLS as u16,
                    ROWS as u16,
                    index == 0,
                    None,
                    None,
                )
                .expect("incremental render");
        }

        let mut full = FrameRenderer::new();
        let mut full_out = Vec::new();
        full.render_to_writer(
            &mut full_out,
            frames.last().expect("non-empty scenario"),
            "status",
            COLS as u16,
            ROWS as u16,
            true,
            None,
            None,
        )
        .expect("full render");

        assert_eq!(
            ansi_bytes_to_rows(&stream, COLS, ROWS),
            ansi_bytes_to_rows(&full_out, COLS, ROWS),
            "diff replay diverged from full render in scenario: {name}"
        );
    }
}

fn tall_frame(cols: usize, rows: &[&str], cursor: (u16, u16)) -> RenderFrame {
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
            rows: rows.iter().map(|row| plain_cells(row)).collect(),
            cursor: (0, 0),
            focused: true,
        }],
        dividers: vec![],
        focused_cursor: Some(cursor),
        focused_cursor_hidden: false,
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
        cursor_color: None,
    }
}

fn split_frame(
    cols: usize,
    left_rows: &[&str],
    right_rows: &[&str],
    cursor: (u16, u16),
) -> RenderFrame {
    let left_width = cols / 2 - 1;
    let divider_x = left_width;
    let right_x = divider_x + 1;
    RenderFrame {
        panes: vec![
            RenderPane {
                pane_id: 1,
                rect: PaneRect {
                    x: 0,
                    y: 0,
                    width: left_width,
                    height: left_rows.len(),
                },
                view_row_origin: 0,
                rows: left_rows.iter().map(|row| plain_cells(row)).collect(),
                cursor: (0, 0),
                focused: true,
            },
            RenderPane {
                pane_id: 2,
                rect: PaneRect {
                    x: right_x,
                    y: 0,
                    width: cols - right_x,
                    height: right_rows.len(),
                },
                view_row_origin: 0,
                rows: right_rows.iter().map(|row| plain_cells(row)).collect(),
                cursor: (0, 0),
                focused: false,
            },
        ],
        dividers: vec![Divider {
            orientation: DividerOrientation::Vertical,
            x: divider_x,
            y: 0,
            len: left_rows.len(),
        }],
        focused_cursor: Some(cursor),
        focused_cursor_hidden: false,
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
        cursor_color: None,
    }
}
