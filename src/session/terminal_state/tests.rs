use std::sync::Arc;
use std::time::Duration;

use crossterm::style::Color;

use super::{CellStyle, MAX_OSC8_URI_LEN, StyledCell, TerminalEvent, TerminalState};

#[test]
fn writes_and_wraps() {
    let mut state = TerminalState::new(4, 2);
    state.feed(b"abcde");
    assert_eq!(state.row_text(0), "abcd");
    assert_eq!(state.row_text(1), "e   ");
}

#[test]
fn bracketed_paste_mode_tracks_decset_2004() {
    let mut state = TerminalState::new(10, 2);
    assert!(!state.bracketed_paste());
    state.feed(b"\x1b[?2004h");
    assert!(state.bracketed_paste());
    state.feed(b"\x1b[?2004l");
    assert!(!state.bracketed_paste());
}

#[test]
fn kitty_keyboard_push_pop_tracks_flags() {
    let mut state = TerminalState::new(10, 2);
    assert_eq!(state.kitty_keyboard_flags(), 0);

    state.feed(b"\x1b[>1u");
    assert_eq!(state.kitty_keyboard_flags(), 1);
    state.feed(b"\x1b[>5u");
    assert_eq!(state.kitty_keyboard_flags(), 5);

    state.feed(b"\x1b[<1u");
    assert_eq!(state.kitty_keyboard_flags(), 1);
    // Pop with omitted count defaults to 1.
    state.feed(b"\x1b[<u");
    assert_eq!(state.kitty_keyboard_flags(), 0);
}

#[test]
fn kitty_keyboard_pop_below_stack_resets_to_zero() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[>1u\x1b[>2u\x1b[<5u");
    assert_eq!(state.kitty_keyboard_flags(), 0);
}

#[test]
fn kitty_keyboard_push_with_omitted_flags_pushes_zero() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[>31u\x1b[>u");
    assert_eq!(state.kitty_keyboard_flags(), 0);
    state.feed(b"\x1b[<u");
    assert_eq!(state.kitty_keyboard_flags(), 31);
}

#[test]
fn kitty_keyboard_set_mode_variants() {
    let mut state = TerminalState::new(10, 2);
    // Mode 1 (assign all bits) is the default.
    state.feed(b"\x1b[=5u");
    assert_eq!(state.kitty_keyboard_flags(), 5);
    // Mode 2 sets the given bits.
    state.feed(b"\x1b[=2;2u");
    assert_eq!(state.kitty_keyboard_flags(), 7);
    // Mode 3 clears the given bits.
    state.feed(b"\x1b[=4;3u");
    assert_eq!(state.kitty_keyboard_flags(), 3);
    // Explicit mode 1 assigns.
    state.feed(b"\x1b[=8;1u");
    assert_eq!(state.kitty_keyboard_flags(), 8);
    // Set acts on the stack top when entries were pushed.
    state.feed(b"\x1b[>1u\x1b[=2;2u");
    assert_eq!(state.kitty_keyboard_flags(), 3);
    state.feed(b"\x1b[<u");
    assert_eq!(state.kitty_keyboard_flags(), 8);
}

#[test]
fn kitty_keyboard_query_reports_current_flags() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[?u");
    assert_eq!(state.drain_responses(), vec![b"\x1b[?0u".to_vec()]);

    state.feed(b"\x1b[>13u\x1b[?u");
    assert_eq!(state.drain_responses(), vec![b"\x1b[?13u".to_vec()]);
}

#[test]
fn kitty_keyboard_stack_cap_evicts_oldest_entry() {
    let mut state = TerminalState::new(10, 2);
    // Push 17 entries (values 1..=17 masked to the defined bits); the
    // cap of 16 evicts the oldest entry (1).
    for flags in 1..=17u8 {
        state.feed(format!("\x1b[>{}u", flags & 0b1_1111).as_bytes());
    }
    assert_eq!(state.kitty_keyboard_flags(), 17);
    // Pop 15 entries: the survivor is entry 2 (entry 1 was evicted).
    state.feed(b"\x1b[<15u");
    assert_eq!(state.kitty_keyboard_flags(), 2);
    state.feed(b"\x1b[<1u");
    assert_eq!(state.kitty_keyboard_flags(), 0);
}

#[test]
fn kitty_keyboard_alt_screen_has_separate_stack() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[>1u");
    assert_eq!(state.kitty_keyboard_flags(), 1);

    // Entering the alternate screen switches to its own (empty) stack.
    state.feed(b"\x1b[?1049h");
    assert_eq!(state.kitty_keyboard_flags(), 0);
    state.feed(b"\x1b[>8u");
    assert_eq!(state.kitty_keyboard_flags(), 8);

    // Leaving the alternate screen restores the main screen's flags.
    state.feed(b"\x1b[?1049l");
    assert_eq!(state.kitty_keyboard_flags(), 1);
}

#[test]
fn kitty_keyboard_flags_are_masked_to_defined_bits() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[>255u");
    assert_eq!(state.kitty_keyboard_flags(), 0b1_1111);
}

#[test]
fn synchronized_output_tracks_decset_2026() {
    let mut state = TerminalState::new(10, 2);
    assert!(!state.synchronized_output_active());
    state.feed(b"\x1b[?2026h");
    assert!(state.synchronized_output_active());
    state.feed(b"\x1b[?2026l");
    assert!(!state.synchronized_output_active());
}

#[test]
fn synchronized_output_hold_expires_after_timeout() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[?2026h");
    state.grid.sync_output_since =
        Some(std::time::Instant::now() - (super::SYNC_OUTPUT_TIMEOUT + Duration::from_millis(50)));
    assert!(!state.synchronized_output_active());
}

#[test]
fn osc133_prompt_marks_track_last_prompt_row() {
    let mut state = TerminalState::new(20, 4);
    assert_eq!(state.last_prompt_abs_row(), None);

    state.feed(b"\x1b]133;A\x07$ echo hi\r\n");
    assert_eq!(state.last_prompt_abs_row(), Some(0));

    state.feed(b"hi\r\n\x1b]133;A\x07$ ");
    assert_eq!(state.last_prompt_abs_row(), Some(2));

    // Non-A marks don't move the prompt row.
    state.feed(b"\x1b]133;C\x07");
    assert_eq!(state.last_prompt_abs_row(), Some(2));
}

#[test]
fn osc10_11_queries_answer_from_cached_host_colors() {
    use crate::io::host_colors::HostColors;

    let mut state = TerminalState::new(10, 2);
    state.set_host_colors(HostColors {
        fg: Some((0xab, 0xcd, 0xef)),
        bg: Some((0x1e, 0x2a, 0x3c)),
    });

    // BEL-terminated query gets a BEL-terminated, 16-bit-per-channel
    // reply with each 8-bit byte doubled.
    state.feed(b"\x1b]10;?\x07");
    assert_eq!(
        state.drain_responses(),
        vec![b"\x1b]10;rgb:abab/cdcd/efef\x07".to_vec()]
    );

    // ST-terminated query gets an ST-terminated reply.
    state.feed(b"\x1b]11;?\x1b\\");
    assert_eq!(
        state.drain_responses(),
        vec![b"\x1b]11;rgb:1e1e/2a2a/3c3c\x1b\\".to_vec()]
    );
}

#[test]
fn osc10_11_queries_stay_silent_without_cached_colors() {
    use crate::io::host_colors::HostColors;

    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b]10;?\x07\x1b]11;?\x1b\\");
    assert!(state.drain_responses().is_empty());

    // A partially known cache only answers the known channel.
    state.set_host_colors(HostColors {
        fg: None,
        bg: Some((0x00, 0x00, 0x00)),
    });
    state.feed(b"\x1b]10;?\x07");
    assert!(state.drain_responses().is_empty());
    state.feed(b"\x1b]11;?\x07");
    assert_eq!(
        state.drain_responses(),
        vec![b"\x1b]11;rgb:0000/0000/0000\x07".to_vec()]
    );
}

#[test]
fn osc10_11_set_forms_are_ignored() {
    use crate::io::host_colors::HostColors;

    let colors = HostColors {
        fg: Some((0xff, 0xff, 0xff)),
        bg: Some((0x00, 0x00, 0x00)),
    };
    let mut state = TerminalState::new(10, 2);
    state.set_host_colors(colors);

    // Guests setting the default colors are ignored in v1: no reply,
    // no crash, and the cached colors stay untouched.
    state.feed(b"\x1b]10;rgb:1111/2222/3333\x07");
    state.feed(b"\x1b]11;#123456\x1b\\");
    state.feed(b"\x1b]10\x07");
    assert!(state.drain_responses().is_empty());
    assert_eq!(state.host_colors(), colors);

    // Queries still answer from the untouched cache afterwards.
    state.feed(b"\x1b]10;?\x07");
    assert_eq!(
        state.drain_responses(),
        vec![b"\x1b]10;rgb:ffff/ffff/ffff\x07".to_vec()]
    );
}

#[test]
fn osc52_clipboard_write_emits_event() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b]52;c;aGVsbG8=\x07");
    let events = state.drain_events();
    assert!(events.contains(&TerminalEvent::ClipboardSet {
        text: "hello".to_string()
    }));

    // ST-terminated form works too.
    state.feed(b"\x1b]52;c;d29ybGQ=\x1b\\");
    let events = state.drain_events();
    assert!(events.contains(&TerminalEvent::ClipboardSet {
        text: "world".to_string()
    }));
}

#[test]
fn osc52_query_and_invalid_payloads_are_ignored() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b]52;c;?\x07");
    state.feed(b"\x1b]52;c;%%%not-base64%%%\x07");
    state.feed(b"\x1b]52;c;\x07");
    state.feed(b"\x1b]52;c\x07");
    assert!(state.drain_events().is_empty());
}

#[test]
fn mouse_protocol_tracks_decset_modes() {
    use super::MouseProtocol;
    let mut state = TerminalState::new(10, 2);
    assert_eq!(state.mouse_protocol(), MouseProtocol::None);
    state.feed(b"\x1b[?1000h");
    assert_eq!(state.mouse_protocol(), MouseProtocol::Normal);
    state.feed(b"\x1b[?1002h");
    assert_eq!(state.mouse_protocol(), MouseProtocol::ButtonEvent);
    state.feed(b"\x1b[?1003h");
    assert_eq!(state.mouse_protocol(), MouseProtocol::AnyEvent);
    state.feed(b"\x1b[?1003l");
    assert_eq!(state.mouse_protocol(), MouseProtocol::None);
    state.feed(b"\x1b[?9h");
    assert_eq!(state.mouse_protocol(), MouseProtocol::X10);
}

#[test]
fn mouse_sgr_encoding_produces_csi_less_than_sequences() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut state = TerminalState::new(80, 24);
    state.feed(b"\x1b[?1002;1006h");

    let press = state
        .encode_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            5,
            3,
        )
        .expect("press encodes");
    assert_eq!(press, b"\x1b[<0;6;4M".to_vec());

    let release = state
        .encode_mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            5,
            3,
        )
        .expect("release encodes");
    assert_eq!(release, b"\x1b[<0;6;4m".to_vec());

    let drag = state
        .encode_mouse_event(
            MouseEventKind::Drag(MouseButton::Right),
            KeyModifiers::NONE,
            0,
            0,
        )
        .expect("drag encodes");
    assert_eq!(drag, b"\x1b[<34;1;1M".to_vec());

    let scroll = state
        .encode_mouse_event(MouseEventKind::ScrollUp, KeyModifiers::CONTROL, 2, 2)
        .expect("scroll encodes");
    assert_eq!(scroll, b"\x1b[<80;3;3M".to_vec());
}

#[test]
fn mouse_legacy_encoding_uses_byte_triplets() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut state = TerminalState::new(80, 24);
    state.feed(b"\x1b[?1000h");

    let press = state
        .encode_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            5,
            3,
        )
        .expect("press encodes");
    assert_eq!(press, vec![0x1b, b'[', b'M', 32, 38, 36]);

    let release = state
        .encode_mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
            5,
            3,
        )
        .expect("release encodes");
    assert_eq!(release, vec![0x1b, b'[', b'M', 35, 38, 36]);
}

#[test]
fn mouse_protocol_filters_event_kinds() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut state = TerminalState::new(80, 24);

    state.feed(b"\x1b[?9h");
    assert!(
        state
            .encode_mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0
            )
            .is_none(),
        "x10 must not report releases"
    );
    assert!(
        state
            .encode_mouse_event(MouseEventKind::ScrollUp, KeyModifiers::NONE, 0, 0)
            .is_none(),
        "x10 must not report scroll"
    );

    state.feed(b"\x1b[?1000h");
    assert!(
        state
            .encode_mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0
            )
            .is_none(),
        "normal protocol must not report drags"
    );

    state.feed(b"\x1b[?1002h");
    assert!(
        state
            .encode_mouse_event(MouseEventKind::Moved, KeyModifiers::NONE, 0, 0)
            .is_none(),
        "button-event protocol must not report plain motion"
    );
    state.feed(b"\x1b[?1003h");
    assert!(
        state
            .encode_mouse_event(MouseEventKind::Moved, KeyModifiers::NONE, 0, 0)
            .is_some(),
        "any-event protocol reports plain motion"
    );
}

#[test]
fn combined_private_modes_apply_all_params() {
    let mut state = TerminalState::new(10, 2);
    state.feed(b"\x1b[?2004;2026h");
    assert!(state.bracketed_paste());
    assert!(state.synchronized_output_active());
    state.feed(b"\x1b[?2004;2026l");
    assert!(!state.bracketed_paste());
    assert!(!state.synchronized_output_active());
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
            style: CellStyle::default(),
            link: None,
        }
    );
}

#[test]
fn sgr_colon_underline_subparams_toggle_underline_only() {
    let mut state = TerminalState::new(8, 1);
    // Curly underline (kitty-style 4:3) must set underline without leaking
    // the style subparameter as an independent SGR code (3 = italic).
    state.feed(b"\x1b[31m\x1b[4:3mA\x1b[4:0mB");

    let row = state.row_cells(0);
    assert!(row[0].style.underlined);
    assert!(!row[0].style.italic);
    // 4:0 turns underline off without resetting other attributes.
    assert!(!row[1].style.underlined);
    assert_eq!(row[1].style.fg, Some(Color::AnsiValue(1)));
}

#[test]
fn sgr_underline_color_arguments_do_not_leak_into_codes() {
    let mut state = TerminalState::new(8, 1);
    // 58 (underline color) arguments must be consumed, not interpreted as
    // codes: flattened, `58:5:4` would enable blink (5) and underline (4).
    state.feed(b"\x1b[58:5:4mA\x1b[58;5;4mB\x1b[58:2::255:4:9mC\x1b[59mD");

    let row = state.row_cells(0);
    for cell in row.iter().take(4) {
        assert_eq!(
            cell.style,
            CellStyle::default(),
            "underline-color SGR must have no attribute side effects: {cell:?}"
        );
    }
}

#[test]
fn sgr_21_is_double_underline_not_bold_off() {
    let mut state = TerminalState::new(4, 1);
    state.feed(b"\x1b[1;21mA\x1b[24mB");

    let row = state.row_cells(0);
    assert!(row[0].style.bold);
    assert!(row[0].style.underlined);
    assert!(!row[1].style.underlined);
}

#[test]
fn claude_code_style_underline_does_not_stick_to_next_line() {
    // Regression: styled underline plus underline color, then colon-form
    // underline off (Claude Code style). The following plain prompt line
    // must not inherit the underline.
    let mut state = TerminalState::new(16, 2);
    state.feed(b"\x1b[4:3m\x1b[58:5:12munderlined\x1b[4:0m\x1b[59m\r\nplain");

    let underlined_row = state.row_cells(0);
    assert!(underlined_row[0].style.underlined);
    let plain_row = state.row_cells(1);
    for cell in plain_row.iter().take(5) {
        assert!(
            !cell.style.underlined,
            "plain prompt line must not keep the underline: {cell:?}"
        );
    }
}

#[test]
fn parses_colon_form_extended_colors() {
    let mut state = TerminalState::new(4, 1);
    state.feed(b"\x1b[38:5:196mA\x1b[0m\x1b[38:2:12:34:56mB\x1b[0m\x1b[38:2::12:34:56mC");

    let row = state.row_cells(0);
    assert_eq!(row[0].style.fg, Some(Color::AnsiValue(196)));
    let rgb = Some(Color::Rgb {
        r: 12,
        g: 34,
        b: 56,
    });
    assert_eq!(row[1].style.fg, rgb);
    // ITU colon form with an (empty) colorspace id.
    assert_eq!(row[2].style.fg, rgb);
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
            style: CellStyle::default(),
            link: None,
        }
    );
    assert_eq!(
        row[1],
        StyledCell {
            ch: 'B',
            style: CellStyle::default(),
            link: None,
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
            link: None,
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
fn osc8_sequences_are_not_forwarded_to_passthrough_queue() {
    // OSC 8 is modelled per-cell (see `osc8_marks_printed_cells_with_hyperlink`)
    // and re-emitted by the renderer aligned with the frame, so the raw guest
    // sequence must NOT also be forwarded to the host. Double-emitting used to
    // leave the host stuck in an open hyperlink state whenever a guest's open
    // and close arrived in different output bursts, bleeding underline/link
    // styling across the whole frame including spectra's own status line.
    let mut state = TerminalState::new(16, 2);
    state.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07");

    assert!(
        state.drain_passthrough().is_empty(),
        "OSC 8 must not be forwarded to the host passthrough channel"
    );
    assert_eq!(state.row_text(0), "link            ");
    // The link is still tracked per-cell for the renderer to emit.
    assert_eq!(state.row_cells(0)[0].link.as_deref(), Some("https://example.com"));
}

#[test]
fn osc8_marks_printed_cells_with_hyperlink() {
    let mut state = TerminalState::new(16, 2);
    state.feed(b"\x1b]8;;https://example.com\x1b\\text\x1b]8;;\x1b\\plain");

    let row = state.row_cells(0);
    for (col, cell) in row.iter().enumerate().take(4) {
        assert_eq!(
            cell.link.as_deref(),
            Some("https://example.com"),
            "cell {col} should carry the OSC 8 link"
        );
    }
    for (col, cell) in row.iter().enumerate().take(9).skip(4) {
        assert_eq!(cell.link, None, "cell {col} should be unlinked");
    }
    // Consecutive cells share the same allocation instead of re-allocating.
    assert!(Arc::ptr_eq(
        row[0].link.as_ref().expect("link"),
        row[3].link.as_ref().expect("link")
    ));
}

#[test]
fn osc8_link_survives_scroll_into_history() {
    let mut state = TerminalState::new(16, 2);
    state.feed(b"\x1b]8;;https://example.com\x1b\\text\x1b]8;;\x1b\\\r\nb\r\nc\r\nd");

    let row = state.absolute_row_cells(0);
    assert_eq!(row[0].ch, 't');
    assert_eq!(row[0].link.as_deref(), Some("https://example.com"));
    assert_eq!(row[3].link.as_deref(), Some("https://example.com"));
}

#[test]
fn osc8_uri_longer_than_cap_is_dropped() {
    let mut state = TerminalState::new(16, 2);
    let sequence = format!(
        "\x1b]8;;https://example.com/{}\x1b\\x",
        "a".repeat(MAX_OSC8_URI_LEN)
    );
    state.feed(sequence.as_bytes());

    let row = state.row_cells(0);
    assert_eq!(row[0].ch, 'x');
    assert_eq!(row[0].link, None);
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

/// Resizing while on the alt screen must reflow the saved primary screen
/// exactly like resizing the primary screen directly would.
#[test]
fn alt_screen_resize_reflows_saved_primary_like_twin() {
    let mut state = TerminalState::new(8, 3);
    let mut twin = TerminalState::new(8, 3);
    for s in [&mut state, &mut twin] {
        s.feed(b"first line!\r\nsecond\r\n$ ");
    }

    state.feed(b"\x1b[?1049h");
    state.feed(b"FULLSCREEN APP");
    state.resize(5, 3);
    twin.resize(5, 3);
    state.feed(b"\x1b[?1049l");

    for row in 0..3 {
        assert_eq!(state.row_text(row), twin.row_text(row), "row {row}");
    }
    assert_eq!(state.history_lines(), twin.history_lines());
    assert_eq!(state.cursor(), twin.cursor());
    let (cx, cy) = state.cursor();
    assert!(cx < state.width() && cy < state.height());
}

/// A soft-wrapped long line on the saved primary screen must survive an
/// alt-screen resize without losing characters.
#[test]
fn alt_screen_resize_preserves_soft_wrapped_saved_line() {
    let mut state = TerminalState::new(6, 2);
    state.feed(b"abcdefghij"); // soft-wraps: "abcdef" / "ghij  "
    state.feed(b"\x1b[?1049h");
    state.resize(4, 2);
    state.feed(b"\x1b[?1049l");

    // scrollback_text() covers scrollback plus the visible rows
    let flattened: String = state
        .scrollback_text()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert_eq!(flattened, "abcdefghij");
}

/// Consecutive resizes while on the alt screen must compose the same way
/// consecutive resizes on the primary screen do.
#[test]
fn alt_screen_shrink_then_grow_matches_twin() {
    let mut state = TerminalState::new(6, 3);
    let mut twin = TerminalState::new(6, 3);
    for s in [&mut state, &mut twin] {
        s.feed(b"hello\r\nabcdef\r\nx");
    }

    state.feed(b"\x1b[?1049h");
    state.resize(3, 2);
    twin.resize(3, 2);
    state.resize(6, 3);
    twin.resize(6, 3);
    state.feed(b"\x1b[?1049l");

    for row in 0..3 {
        assert_eq!(state.row_text(row), twin.row_text(row), "row {row}");
    }
    assert_eq!(state.history_lines(), twin.history_lines());
    assert_eq!(state.cursor(), twin.cursor());
}

/// The alt screen itself keeps clip/pad semantics on resize: growing pads
/// with blanks, it never reflows (fullscreen apps repaint themselves).
#[test]
fn alt_screen_resize_clips_and_pads_alt_content() {
    let mut state = TerminalState::new(3, 2);
    state.feed(b"\x1b[?1049h");
    state.feed(b"XYZAB"); // row0="XYZ" (soft wrap), row1="AB "
    state.resize(6, 3);

    // No reflow on the alt screen: rows are padded in place
    assert_eq!(state.row_text(0), "XYZ   ");
    assert_eq!(state.row_text(1), "AB    ");
    assert_eq!(state.row_text(2), "      ");
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
