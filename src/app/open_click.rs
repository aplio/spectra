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
        let side_window_tree = self.side_window_tree_overlay();
        let frame = self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
        let Some(pane) = Self::mouse_pane_info_at(&frame, col, row) else {
            return;
        };
        let pane_id = pane.pane_id;
        let local_col = usize::from(col)
            .saturating_sub(pane.rect.x)
            .min(pane.rect.width.saturating_sub(1));
        let local_row = usize::from(row)
            .saturating_sub(pane.rect.y)
            .min(pane.rect.height.saturating_sub(1));
        let absolute_row = pane.view_row_origin.saturating_add(local_row);

        let Some((text, click_offset, link)) =
            self.open_click_row_text(pane_id, absolute_row, local_col)
        else {
            self.set_message("nothing to open here", Duration::from_secs(2));
            return;
        };

        let cwd = self.current_session().pane_cwd(pane_id);
        let target = match link {
            Some(link) => target_for_link(&link),
            None => target_at(&text, click_offset, cwd.as_deref()),
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

    /// Text of the logical line containing `absolute_row` (soft-wrapped
    /// neighbours joined, bounded by [`MAX_WRAP_JOIN_ROWS`]), the byte
    /// offset of the clicked cell within it, and the clicked cell's OSC 8
    /// hyperlink when it carries one.
    fn open_click_row_text(
        &self,
        pane_id: usize,
        absolute_row: usize,
        local_col: usize,
    ) -> Option<(String, usize, Option<String>)> {
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
        let mut click_offset = None;
        for row in start_row..=end_row {
            let cells = if row == absolute_row {
                clicked_cells.clone()
            } else {
                session
                    .pane_absolute_row_cells(pane_id, row)
                    .unwrap_or_default()
            };
            for (col, cell) in cells.iter().enumerate() {
                if row == absolute_row && col == local_col {
                    click_offset = Some(text.len());
                }
                if cell.ch != '\0' {
                    text.push(cell.ch);
                }
            }
        }
        Some((text, click_offset?, link))
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
/// `cwd`).
fn target_at(text: &str, offset: usize, cwd: Option<&Path>) -> Option<OpenClickTarget> {
    if let Some(span) = crate::ui::url::find_web_url_spans(text)
        .into_iter()
        .find(|span| span.contains_byte(offset))
    {
        return Some(OpenClickTarget::Url(span.as_str(text).to_string()));
    }

    let token = path_token_at(text, offset)?;
    for (candidate, line) in path_candidates(&token) {
        if let Some(resolved) = resolve_path_candidate(&candidate, cwd)
            && let Some(target) = classify_existing_path(resolved, line)
        {
            return Some(target);
        }
    }
    None
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

/// The maximal run of path characters around byte `offset`.
fn path_token_at(text: &str, offset: usize) -> Option<String> {
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
    (!token.is_empty()).then(|| token.to_string())
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
            path_token_at(text, offset).as_deref(),
            Some("src/main.rs:42:7")
        );
    }

    #[test]
    fn token_extraction_handles_quotes_and_prose() {
        let text = "open \"~/notes/todo.md\" next";
        let offset = text.find("todo").unwrap();
        assert_eq!(
            path_token_at(text, offset).as_deref(),
            Some("~/notes/todo.md")
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

        let offset = text.find("file.rs").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some(OpenClickTarget::File {
                path: dir.path().join("sub/file.rs"),
                line: Some(3),
            })
        );

        let offset = text.find("sub/ ").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some(OpenClickTarget::Dir(dir.path().join("sub")))
        );

        let offset = text.find("example").unwrap();
        assert_eq!(
            target_at(text, offset, cwd),
            Some(OpenClickTarget::Url("https://example.com".to_string()))
        );

        assert_eq!(target_at(text, text.find("now").unwrap(), cwd), None);
        assert_eq!(target_at(text, text.find("and").unwrap(), cwd), None);
    }
}
