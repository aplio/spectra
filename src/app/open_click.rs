use super::*;

use crate::ui::layout::SplitAxis;

/// What a modifier+click on pane text resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpenClickTarget {
    Dir(PathBuf),
    File { path: PathBuf, line: Option<usize> },
    Url(String),
}

/// Rows joined around the clicked one when it soft-wraps, in each
/// direction. Bounds the text scanned for a path so a pane full of one
/// giant wrapped line stays cheap.
const MAX_WRAP_JOIN_ROWS: usize = 16;

/// The logical line around a clicked/hovered cell, plus enough cell
/// bookkeeping to map a byte span in `text` back onto screen cells.
struct OpenClickContext {
    text: String,
    /// Byte offset of the clicked cell within `text`.
    click_offset: usize,
    /// OSC 8 hyperlink carried by the clicked cell.
    link: Option<String>,
    /// Cells of the clicked row sharing that hyperlink, as
    /// (absolute row, contiguous column range).
    link_run: Option<(usize, std::ops::Range<usize>)>,
    /// (byte offset in `text`, absolute row, pane-local column) for every
    /// cell that contributed; wide-char continuation cells repeat their
    /// owner's offset.
    cells: Vec<(usize, usize, usize)>,
}

impl App {
    /// Whether `modifiers` carries the `[mouse] open_click` modifier.
    pub(super) fn open_click_modifier_matches(
        &self,
        modifiers: crossterm::event::KeyModifiers,
    ) -> bool {
        use crossterm::event::KeyModifiers;
        let required = match self.open_click {
            config::OpenClickModifier::Off => return false,
            config::OpenClickModifier::Ctrl => KeyModifiers::CONTROL,
            config::OpenClickModifier::Alt => KeyModifiers::ALT,
            config::OpenClickModifier::Shift => KeyModifiers::SHIFT,
            config::OpenClickModifier::Super => KeyModifiers::SUPER,
        };
        modifiers.contains(required)
    }

    /// Open whatever sits under a modifier+click (ghostty's cmd+click): an
    /// existing directory becomes a new pane cwd'd there, an existing file
    /// opens in the editor, and a URL (plain text or OSC 8 hyperlink) opens
    /// in the system browser. The click is always consumed; when nothing
    /// resolves a status message says so instead of degrading into a stray
    /// selection or guest click.
    pub(super) fn handle_open_click(&mut self, col: u16, row: u16) {
        self.needs_render = true;
        self.view.open_click_hover = None;
        let Some((pane_id, absolute_row, local_col)) = self.open_click_position(col, row) else {
            return;
        };

        let Some(ctx) = self.open_click_context(pane_id, absolute_row, local_col) else {
            self.set_message("nothing to open here", Duration::from_secs(2));
            return;
        };

        let cwd = self.current_session().pane_cwd(pane_id);
        let target = match &ctx.link {
            Some(link) => target_for_link(link),
            None => {
                target_at(&ctx.text, ctx.click_offset, cwd.as_deref()).map(|(target, _)| target)
            }
        };
        let Some(target) = target else {
            self.set_message("no path or url under cursor", Duration::from_secs(2));
            return;
        };
        match target {
            OpenClickTarget::Dir(path) => self.open_click_dir(pane_id, path),
            OpenClickTarget::File { path, line } => self.open_path_in_editor(&path, line),
            OpenClickTarget::Url(url) => self.open_click_url(&url),
        }
    }

    /// Resolve screen coordinates to the pane under them and the buffer cell
    /// they land on, as (pane id, absolute row, pane-local column).
    fn open_click_position(&self, col: u16, row: u16) -> Option<(usize, usize, usize)> {
        let side_window_tree = self.side_window_tree_overlay();
        let frame = self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
        let pane = Self::mouse_pane_info_at(&frame, col, row)?;
        let local_col = usize::from(col)
            .saturating_sub(pane.rect.x)
            .min(pane.rect.width.saturating_sub(1));
        let local_row = usize::from(row)
            .saturating_sub(pane.rect.y)
            .min(pane.rect.height.saturating_sub(1));
        let absolute_row = pane.view_row_origin.saturating_add(local_row);
        Some((pane.pane_id, absolute_row, local_col))
    }

    /// Track what an open-click at the pointer would hit while the modifier
    /// is held, underlining it via [`ClientViewState::open_click_hover`]
    /// (ghostty's cmd+hover affordance). Any other event clears the
    /// underline; motion without a resolvable target does too.
    pub(super) fn update_open_click_hover(&mut self, mouse: &crossterm::event::MouseEvent) {
        let hover = if matches!(self.view.input_mode, InputMode::Normal)
            && matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
            && self.open_click_modifier_matches(mouse.modifiers)
        {
            self.open_click_hover_at(mouse.column, mouse.row)
        } else {
            None
        };
        if hover != self.view.open_click_hover {
            self.view.open_click_hover = hover;
            self.needs_render = true;
        }
    }

    /// The cells an open-click at screen (col, row) would open, or `None`
    /// when nothing under the pointer resolves.
    fn open_click_hover_at(&self, col: u16, row: u16) -> Option<OpenClickHoverState> {
        let (pane_id, absolute_row, local_col) = self.open_click_position(col, row)?;
        let ctx = self.open_click_context(pane_id, absolute_row, local_col)?;
        if let Some(link) = &ctx.link {
            target_for_link(link)?;
            let (link_row, cols) = ctx.link_run?;
            return Some(OpenClickHoverState {
                pane_id,
                rows: vec![(link_row, cols)],
            });
        }
        let cwd = self.current_session().pane_cwd(pane_id);
        let (_, span) = target_at(&ctx.text, ctx.click_offset, cwd.as_deref())?;
        let rows = hover_rows_for_span(&ctx.cells, span);
        (!rows.is_empty()).then_some(OpenClickHoverState { pane_id, rows })
    }

    /// Text of the logical line containing `absolute_row` (soft-wrapped
    /// neighbours joined, bounded by [`MAX_WRAP_JOIN_ROWS`]), the byte
    /// offset of the clicked cell within it, the clicked cell's OSC 8
    /// hyperlink when it carries one, and the text-to-cell mapping.
    fn open_click_context(
        &self,
        pane_id: usize,
        absolute_row: usize,
        local_col: usize,
    ) -> Option<OpenClickContext> {
        let session = self.current_session();
        let clicked_cells = session.pane_absolute_row_cells(pane_id, absolute_row)?;
        if clicked_cells.is_empty() {
            return None;
        }
        // A click on a wide char's continuation cell belongs to its owner.
        let mut local_col = local_col.min(clicked_cells.len() - 1);
        while local_col > 0
            && clicked_cells
                .get(local_col)
                .is_some_and(|cell| cell.ch == '\0')
        {
            local_col -= 1;
        }
        let link = clicked_cells
            .get(local_col)
            .and_then(|cell| cell.link.as_ref())
            .map(|link| link.to_string());
        let link_run = link.as_ref().map(|link| {
            let same_link = |col: usize| {
                clicked_cells[col]
                    .link
                    .as_ref()
                    .is_some_and(|l| l.as_ref() == link)
            };
            let mut from = local_col;
            while from > 0 && same_link(from - 1) {
                from -= 1;
            }
            let mut to = local_col + 1;
            while to < clicked_cells.len() && same_link(to) {
                to += 1;
            }
            (absolute_row, from..to)
        });

        let mut start_row = absolute_row;
        for _ in 0..MAX_WRAP_JOIN_ROWS {
            if start_row == 0
                || !session
                    .pane_absolute_row_soft_wrapped(pane_id, start_row - 1)
                    .unwrap_or(false)
            {
                break;
            }
            start_row -= 1;
        }
        let mut end_row = absolute_row;
        for _ in 0..MAX_WRAP_JOIN_ROWS {
            if !session
                .pane_absolute_row_soft_wrapped(pane_id, end_row)
                .unwrap_or(false)
            {
                break;
            }
            end_row += 1;
        }

        let mut text = String::new();
        let mut cell_offsets = Vec::new();
        let mut click_offset = None;
        for row in start_row..=end_row {
            let cells = if row == absolute_row {
                clicked_cells.clone()
            } else {
                session
                    .pane_absolute_row_cells(pane_id, row)
                    .unwrap_or_default()
            };
            let mut last_offset = None;
            for (col, cell) in cells.iter().enumerate() {
                if row == absolute_row && col == local_col {
                    click_offset = Some(text.len());
                }
                let offset = if cell.ch != '\0' {
                    let offset = text.len();
                    text.push(cell.ch);
                    Some(offset)
                } else {
                    // A continuation cell highlights with its owner char.
                    last_offset
                };
                if let Some(offset) = offset {
                    cell_offsets.push((offset, row, col));
                    last_offset = Some(offset);
                }
            }
        }
        Some(OpenClickContext {
            text,
            click_offset: click_offset?,
            link,
            link_run,
            cells: cell_offsets,
        })
    }

    fn open_click_dir(&mut self, pane_id: usize, path: PathBuf) {
        let (cols, rows) = self.current_effective_pane_dims();
        if self.current_session().focused_pane_id() != Some(pane_id)
            && self.current_session_mut().focus_pane_id(pane_id).is_ok()
        {
            self.record_focus_for_active_session();
        }
        match self.current_session_mut().split_focused_with_cwd(
            SplitAxis::Vertical,
            cols,
            rows,
            path.clone(),
        ) {
            Ok(()) => {
                self.apply_action_effects(ActionEffects::structure(HookEvent::PaneSplit));
                self.write_log("opened directory pane via click");
                self.set_message(
                    &format!("opened pane in {}", path.display()),
                    Duration::from_secs(3),
                );
            }
            Err(err) => {
                self.set_message(&format!("open dir failed: {err}"), Duration::from_secs(3));
            }
        }
    }

    fn open_click_url(&mut self, url: &str) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reap the opener off-thread so it never lingers as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                self.set_message(&format!("opened {url}"), Duration::from_secs(2));
            }
            Err(err) => {
                self.set_message(
                    &format!("open url failed ({opener}): {err}"),
                    Duration::from_secs(3),
                );
            }
        }
    }
}

/// Resolve an OSC 8 hyperlink: `file://` targets open as paths, web URLs go
/// to the browser, anything else (mailto etc.) is declined.
fn target_for_link(link: &str) -> Option<OpenClickTarget> {
    if let Some(rest) = strip_prefix_ignore_case(link, "file://") {
        // Skip the authority (usually empty or a hostname) up to the path.
        let path = if rest.starts_with('/') {
            rest
        } else {
            &rest[rest.find('/')?..]
        };
        return classify_existing_path(PathBuf::from(percent_decode(path)), None);
    }
    if strip_prefix_ignore_case(link, "http://").is_some()
        || strip_prefix_ignore_case(link, "https://").is_some()
    {
        return Some(OpenClickTarget::Url(link.to_string()));
    }
    None
}

/// Resolve the token under `offset` in `text` to an open target: a web URL
/// span, or a path candidate that exists on disk (relative ones against
/// `cwd`). Also returns the byte span in `text` the target came from, so a
/// hover can underline exactly what a click would open.
fn target_at(
    text: &str,
    offset: usize,
    cwd: Option<&Path>,
) -> Option<(OpenClickTarget, std::ops::Range<usize>)> {
    if let Some(span) = crate::ui::url::find_web_url_spans(text)
        .into_iter()
        .find(|span| span.contains_byte(offset))
    {
        return Some((
            OpenClickTarget::Url(span.as_str(text).to_string()),
            span.start..span.end,
        ));
    }

    let (token, token_start) = path_token_at(text, offset)?;
    for (candidate, line) in path_candidates(&token) {
        if let Some(resolved) = resolve_path_candidate(&candidate, cwd)
            && let Some(target) = classify_existing_path(resolved, line)
        {
            // The span keeps trimmed trailing punctuation out but always
            // covers the `:line` suffix and a stripped `a/` diff prefix, so
            // the underline reads as one token.
            let span_len = token.trim_end_matches(['.', ',', ';', ':', '!']).len();
            return Some((
                target,
                token_start..token_start + span_len.max(candidate.len()),
            ));
        }
    }
    None
}

/// Map a byte span of the joined logical line back onto screen cells, as
/// (absolute row, contiguous pane-local column range) per touched row.
fn hover_rows_for_span(
    cells: &[(usize, usize, usize)],
    span: std::ops::Range<usize>,
) -> Vec<(usize, std::ops::Range<usize>)> {
    let mut rows: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
    for &(offset, row, col) in cells {
        if !span.contains(&offset) {
            continue;
        }
        match rows.last_mut() {
            Some((last_row, range)) if *last_row == row && range.end == col => {
                range.end = col + 1;
            }
            _ => rows.push((row, col..col + 1)),
        }
    }
    rows
}

fn classify_existing_path(path: PathBuf, line: Option<usize>) -> Option<OpenClickTarget> {
    if path.is_dir() {
        Some(OpenClickTarget::Dir(path))
    } else if path.is_file() {
        Some(OpenClickTarget::File { path, line })
    } else {
        None
    }
}

/// Characters that can be part of a clicked path token. Quotes, brackets
/// and common separators end the token so paths embedded in prose, compiler
/// output or listings come out clean; `:` stays in for `file:line` suffixes.
fn is_path_char(c: char) -> bool {
    if c.is_whitespace() {
        return false;
    }
    !matches!(
        c,
        '"' | '\''
            | '`'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | '|'
            | ';'
            | ','
            | '*'
            | '?'
            | '='
            | '（'
            | '）'
            | '「'
            | '」'
            | '。'
            | '、'
            | '：'
    )
}

/// The maximal run of path characters around byte `offset`, with its byte
/// start in `text`.
fn path_token_at(text: &str, offset: usize) -> Option<(String, usize)> {
    if offset >= text.len() {
        return None;
    }
    let mut start = offset;
    while start > 0 {
        let prev = text[..start].chars().next_back()?;
        if !is_path_char(prev) {
            break;
        }
        start -= prev.len_utf8();
    }
    let mut end = offset;
    for c in text[offset..].chars() {
        if !is_path_char(c) {
            break;
        }
        end += c.len_utf8();
    }
    let token = &text[start..end];
    (!token.is_empty()).then(|| (token.to_string(), start))
}

/// Interpretations of a token to try against the filesystem, in order:
/// verbatim, with trailing punctuation trimmed, with a `:line[:col]` suffix
/// split off, and each of those without a git-diff `a/`/`b/` prefix.
fn path_candidates(token: &str) -> Vec<(String, Option<usize>)> {
    fn push(out: &mut Vec<(String, Option<usize>)>, candidate: &str, line: Option<usize>) {
        if !candidate.is_empty()
            && candidate != "."
            && !out.iter().any(|(existing, _)| existing == candidate)
        {
            out.push((candidate.to_string(), line));
        }
    }

    let mut out: Vec<(String, Option<usize>)> = Vec::new();
    push(&mut out, token, None);
    let trimmed = token.trim_end_matches(['.', ',', ';', ':', '!']);
    push(&mut out, trimmed, None);
    if let Some((base, line)) = split_line_suffix(trimmed) {
        push(&mut out, base, Some(line));
    }
    for index in 0..out.len() {
        let (candidate, line) = out[index].clone();
        if let Some(rest) = candidate
            .strip_prefix("a/")
            .or_else(|| candidate.strip_prefix("b/"))
        {
            push(&mut out, rest, line);
        }
    }
    out
}

/// Split a trailing `:line` or `:line:col` off a token, returning the base
/// and the line number.
fn split_line_suffix(token: &str) -> Option<(&str, usize)> {
    let mut base = token;
    let mut line = None;
    for _ in 0..2 {
        let Some(index) = base.rfind(':') else { break };
        let digits = &base[index + 1..];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        line = digits.parse::<usize>().ok();
        base = &base[..index];
    }
    let line = line?;
    (!base.is_empty()).then_some((base, line))
}

fn resolve_path_candidate(candidate: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let expanded = if candidate == "~" {
        PathBuf::from(std::env::var_os("HOME")?)
    } else if let Some(rest) = candidate.strip_prefix("~/") {
        Path::new(&std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(candidate)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(cwd?.join(expanded))
    }
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

/// Minimal percent-decoding for `file://` URL paths; invalid escapes pass
/// through literally.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            )
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_extraction_stops_at_delimiters() {
        let text = "error in (src/main.rs:42:7): expected";
        let offset = text.find("main").unwrap();
        assert_eq!(
            path_token_at(text, offset),
            Some(("src/main.rs:42:7".to_string(), text.find("src").unwrap()))
        );
    }

    #[test]
    fn token_extraction_handles_quotes_and_prose() {
        let text = "open \"~/notes/todo.md\" next";
        let offset = text.find("todo").unwrap();
        assert_eq!(
            path_token_at(text, offset),
            Some(("~/notes/todo.md".to_string(), text.find('~').unwrap()))
        );
        assert_eq!(path_token_at("  ", 1), None);
    }

    #[test]
    fn line_suffix_split() {
        assert_eq!(split_line_suffix("a.rs:42"), Some(("a.rs", 42)));
        assert_eq!(split_line_suffix("a.rs:42:7"), Some(("a.rs", 42)));
        assert_eq!(split_line_suffix("a.rs"), None);
        assert_eq!(split_line_suffix(":42"), None);
    }

    #[test]
    fn candidates_cover_punctuation_line_and_diff_prefixes() {
        let candidates = path_candidates("a/src/lib.rs:10,");
        let names: Vec<&str> = candidates.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"a/src/lib.rs:10,"));
        assert!(names.contains(&"a/src/lib.rs:10"));
        assert!(names.contains(&"a/src/lib.rs"));
        assert!(names.contains(&"src/lib.rs"));
        assert!(
            candidates
                .iter()
                .any(|(name, line)| name == "src/lib.rs" && *line == Some(10))
        );
    }

    #[test]
    fn resolves_relative_against_cwd_and_tilde_against_home() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path_candidate("src/lib.rs", Some(cwd)),
            Some(PathBuf::from("/work/src/lib.rs"))
        );
        assert_eq!(
            resolve_path_candidate("/etc/hosts", None),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(resolve_path_candidate("src/lib.rs", None), None);
    }

    #[test]
    fn link_targets_classify_by_scheme() {
        assert_eq!(
            target_for_link("https://example.com/x"),
            Some(OpenClickTarget::Url("https://example.com/x".to_string()))
        );
        assert_eq!(target_for_link("mailto:x@example.com"), None);
    }

    #[test]
    fn file_links_percent_decode_and_check_existence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spaced = dir.path().join("with space");
        std::fs::create_dir(&spaced).expect("mkdir");
        let encoded = format!("file://{}/with%20space", dir.path().to_string_lossy());
        assert_eq!(
            target_for_link(&encoded),
            Some(OpenClickTarget::Dir(spaced))
        );
    }

    #[test]
    fn target_at_finds_urls_files_and_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/file.rs"), "x").expect("write");

        let text = "see sub/file.rs:3 and sub/ or https://example.com now";
        let cwd = Some(dir.path());

        let file_start = text.find("sub/file").unwrap();
        let offset = text.find("file.rs").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some((
                OpenClickTarget::File {
                    path: dir.path().join("sub/file.rs"),
                    line: Some(3),
                },
                file_start..file_start + "sub/file.rs:3".len(),
            ))
        );

        let offset = text.find("sub/ ").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some((
                OpenClickTarget::Dir(dir.path().join("sub")),
                offset..offset + "sub/".len(),
            ))
        );

        let offset = text.find("example").unwrap();
        let url_start = text.find("https").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some((
                OpenClickTarget::Url("https://example.com".to_string()),
                url_start..url_start + "https://example.com".len(),
            ))
        );

        assert_eq!(target_at(text, text.find("now").unwrap(), cwd), None);
        assert_eq!(target_at(text, text.find("and").unwrap(), cwd), None);
    }

    #[test]
    fn hover_rows_group_contiguous_cells_per_row() {
        // Joined text "abcdef" split over two rows, with a wide char: offsets
        // 2 and 3 map to (row 10, cols 2..4) and (row 11, col 0).
        let cells = vec![
            (0, 10, 0),
            (1, 10, 1),
            (2, 10, 2),
            (2, 10, 3), // continuation cell of the wide char at offset 2
            (3, 11, 0),
            (4, 11, 1),
        ];
        assert_eq!(
            hover_rows_for_span(&cells, 2..4),
            vec![(10, 2..4), (11, 0..1)]
        );
        assert_eq!(hover_rows_for_span(&cells, 6..8), vec![]);
    }
}
