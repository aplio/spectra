use unicode_width::UnicodeWidthChar;

pub const TAB_DISPLAY_WIDTH: usize = 4;

#[inline]
pub fn char_display_width(ch: char) -> usize {
    if ch == '\t' {
        TAB_DISPLAY_WIDTH
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayWindow<'a> {
    pub visible: &'a str,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_col: usize,
    pub used_width: usize,
}

/// Truncate `s` to fit within `max_width` terminal columns.
/// Returns the truncated `&str` slice and its display width.
pub fn truncate_to_width(s: &str, max_width: usize) -> (&str, usize) {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        let cw = char_display_width(c);
        if width + cw > max_width {
            return (&s[..i], width);
        }
        width += cw;
    }
    (s, width)
}

/// Display width of a string in terminal columns.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

/// Slice a display window from `s` starting at display column `start_col`.
pub fn slice_display_window(s: &str, start_col: usize, max_width: usize) -> DisplayWindow<'_> {
    let mut col = 0usize;
    let mut start_byte = s.len();
    let mut effective_start_col = 0usize;

    for (i, ch) in s.char_indices() {
        let ch_w = char_display_width(ch);
        if col >= start_col {
            start_byte = i;
            effective_start_col = col;
            break;
        }
        if col + ch_w > start_col {
            // Cannot render half of a wide character; skip it.
            col += ch_w;
            continue;
        }
        col += ch_w;
    }

    if start_byte == s.len() {
        effective_start_col = col;
    }

    if max_width == 0 || start_byte == s.len() {
        return DisplayWindow {
            visible: "",
            start_byte,
            end_byte: start_byte,
            start_col: effective_start_col,
            used_width: 0,
        };
    }

    let mut used_width = 0usize;
    let mut end_byte = s.len();
    for (i, ch) in s[start_byte..].char_indices() {
        let ch_w = char_display_width(ch);
        if used_width + ch_w > max_width {
            end_byte = start_byte + i;
            break;
        }
        used_width += ch_w;
    }

    DisplayWindow {
        visible: &s[start_byte..end_byte],
        start_byte,
        end_byte,
        start_col: effective_start_col,
        used_width,
    }
}

/// Compute gutter width (line numbers + space) for given total lines.
pub fn gutter_width(total_lines: usize) -> usize {
    let digits = if total_lines == 0 {
        1
    } else {
        (total_lines as f64).log10().floor() as usize + 1
    };
    digits + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_within_limit() {
        let (s, w) = truncate_to_width("hello", 10);
        assert_eq!(s, "hello");
        assert_eq!(w, 5);
    }

    #[test]
    fn truncate_ascii_exact() {
        let (s, w) = truncate_to_width("hello", 5);
        assert_eq!(s, "hello");
        assert_eq!(w, 5);
    }

    #[test]
    fn truncate_ascii_over() {
        let (s, w) = truncate_to_width("hello world", 5);
        assert_eq!(s, "hello");
        assert_eq!(w, 5);
    }

    #[test]
    fn truncate_cjk_no_panic() {
        let (s, w) = truncate_to_width("あいう", 5);
        assert_eq!(s, "あい");
        assert_eq!(w, 4);
    }

    #[test]
    fn truncate_mixed_ascii_cjk() {
        let (s, w) = truncate_to_width("abcあいう", 7);
        assert_eq!(s, "abcあい");
        assert_eq!(w, 7);
    }

    #[test]
    fn truncate_mixed_boundary_between_cjk() {
        let (s, w) = truncate_to_width("abcあいう", 8);
        assert_eq!(s, "abcあい");
        assert_eq!(w, 7);
    }

    #[test]
    fn truncate_zero_width() {
        let (s, w) = truncate_to_width("あいう", 0);
        assert_eq!(s, "");
        assert_eq!(w, 0);
    }

    #[test]
    fn truncate_real_world_japanese_line() {
        let line = "  - 吾輩は猫である。名前はまだ無い。\
                     どこで生まれたかとんと見当がつかぬ。\
                     何でも薄暗いじめじめした所でニャーニャー泣いていた事だけは記憶している。";
        let available = 116;
        let (truncated, w) = truncate_to_width(line, available);
        assert!(w <= available);
        assert!(truncated.len() <= line.len());
    }

    #[test]
    fn display_width_cjk() {
        assert_eq!(display_width("あいう"), 6);
    }

    #[test]
    fn display_width_mixed() {
        assert_eq!(display_width("abcあ"), 5);
    }

    #[test]
    fn display_width_empty() {
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn display_width_tab_counts_as_configured_columns() {
        assert_eq!(display_width("\t"), TAB_DISPLAY_WIDTH);
        assert_eq!(display_width("a\tb"), TAB_DISPLAY_WIDTH + 2);
    }

    #[test]
    fn slice_display_window_ascii() {
        let window = slice_display_window("abcdef", 2, 3);
        assert_eq!(window.visible, "cde");
        assert_eq!(window.start_col, 2);
        assert_eq!(window.used_width, 3);
    }

    #[test]
    fn slice_display_window_skips_partial_wide_char() {
        let window = slice_display_window("abあいう", 3, 4);
        assert_eq!(window.visible, "いう");
        assert_eq!(window.start_col, 4);
        assert_eq!(window.used_width, 4);
    }

    #[test]
    fn slice_display_window_starts_at_tab_boundary() {
        let window = slice_display_window("a\tb", 1, TAB_DISPLAY_WIDTH + 1);
        assert_eq!(window.visible, "\tb");
        assert_eq!(window.start_col, 1);
        assert_eq!(window.used_width, TAB_DISPLAY_WIDTH + 1);
    }

    #[test]
    fn gutter_width_for_zero_lines() {
        assert_eq!(gutter_width(0), 2);
    }
}
