use spectra::session::manager::{RenderFrame, RenderPane};
use spectra::session::terminal_state::StyledCell;
use spectra::ui::render::FrameRenderer;
use spectra::ui::window_manager::PaneRect;

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
        cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
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
