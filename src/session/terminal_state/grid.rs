use super::modes::KITTY_KBD_ALL_FLAGS;
use super::*;

impl TerminalGrid {
    pub(super) fn new(width: usize, height: usize, allow_passthrough: bool) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![StyledCell::default(); width * height],
            scrollback: Vec::new(),
            row_boundaries: vec![RowBoundary::None; height],
            scroll_top: 0,
            scroll_bottom: height.saturating_sub(1),
            cursor_x: 0,
            cursor_y: 0,
            active_style: CellStyle::default(),
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            saved_style: CellStyle::default(),
            cursor_style: crossterm::cursor::SetCursorStyle::DefaultUserShape,
            saved_screen: None,
            response_queue: Vec::new(),
            insert_mode: false,
            bracketed_paste: false,
            sync_output_since: None,
            last_prompt_abs_row: None,
            mouse_protocol: MouseProtocol::None,
            mouse_sgr: false,
            allow_passthrough,
            passthrough_queue: Vec::new(),
            terminal_events: Vec::new(),
            host_colors: HostColors::default(),
            active_link: None,
            kitty_kbd_main: KittyKeyboardStack::default(),
            kitty_kbd_alt: KittyKeyboardStack::default(),
        }
    }

    pub(super) fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.allow_passthrough = allow_passthrough;
        if !allow_passthrough {
            self.passthrough_queue.clear();
        }
    }

    pub(super) fn row_text(&self, row: usize) -> String {
        if row >= self.height {
            return String::new();
        }
        let start = row * self.width;
        let end = start + self.width;
        self.cells[start..end]
            .iter()
            .filter(|cell| cell.ch != '\0')
            .map(|cell| cell.ch)
            .collect()
    }

    pub(super) fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        if row >= self.height {
            return Vec::new();
        }
        let start = row * self.width;
        let end = start + self.width;
        self.cells[start..end].to_vec()
    }

    fn trimmed_row_text(&self, row: usize) -> String {
        self.row_text(row).trim_end_matches(' ').to_string()
    }

    pub(super) fn scrollback_text(&self) -> String {
        self.history_lines().join("\n")
    }

    pub(super) fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    pub(super) fn history_lines(&self) -> Vec<String> {
        let mut lines = self
            .scrollback
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        lines.extend((0..self.height).map(|row| self.trimmed_row_text(row)));
        lines
    }

    pub(super) fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        let mut rows = self
            .scrollback
            .iter()
            .map(|line| self.fit_cells_to_width(&line.cells))
            .collect::<Vec<_>>();
        rows.extend((0..self.height).map(|row| self.row_cells(row)));
        rows
    }

    pub(super) fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }

        let visible_lines = self.height;
        let total_lines = self.scrollback.len() + visible_lines;
        let keep = total_lines.min(max_lines);
        let scrollback_keep = keep.saturating_sub(visible_lines);
        let visible_start = visible_lines.saturating_sub(keep);

        let mut lines = Vec::with_capacity(keep);
        if scrollback_keep > 0 {
            let start = self.scrollback.len().saturating_sub(scrollback_keep);
            lines.extend(
                self.scrollback[start..]
                    .iter()
                    .map(|line| line.text.clone()),
            );
        }
        lines.extend((visible_start..visible_lines).map(|row| self.trimmed_row_text(row)));
        lines
    }

    pub(super) fn export_text_hard_lf(&self) -> String {
        let mut out = String::new();
        for line in &self.scrollback {
            out.push_str(&line.text);
            if line.boundary_to_next == RowBoundary::HardLf {
                out.push('\n');
            }
        }
        for row in 0..self.height {
            out.push_str(&self.trimmed_row_text(row));
            if self.row_boundary_to_next(row) == RowBoundary::HardLf {
                out.push('\n');
            }
        }
        out
    }

    pub(super) fn total_lines(&self) -> usize {
        self.height + self.scrollback.len()
    }

    pub(super) fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        if self.total_lines() <= absolute_row {
            return vec![StyledCell::default(); self.width];
        }

        let history_len = self.scrollback.len();
        if absolute_row < history_len {
            return self.fit_cells_to_width(&self.scrollback[absolute_row].cells);
        }

        let visible_row = absolute_row - history_len;
        self.row_cells(visible_row)
    }

    fn fit_cells_to_width(&self, cells: &[StyledCell]) -> Vec<StyledCell> {
        Self::fit_cells(cells, self.width)
    }

    fn fit_cells(cells: &[StyledCell], width: usize) -> Vec<StyledCell> {
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

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    fn clear_row(&mut self, row: usize) {
        if row >= self.height {
            return;
        }
        let row_start = row * self.width;
        self.cells[row_start..row_start + self.width].fill(StyledCell::default());
        self.row_boundaries[row] = RowBoundary::None;
    }

    fn copy_row(&mut self, dst: usize, src: usize) {
        if dst >= self.height || src >= self.height {
            return;
        }
        let src_start = src * self.width;
        let dst_start = dst * self.width;
        for x in 0..self.width {
            self.cells[dst_start + x] = self.cells[src_start + x].clone();
        }
        self.row_boundaries[dst] = self.row_boundary_to_next(src);
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let last_row = self.height.saturating_sub(1);
        let top = top.min(last_row);
        let bottom = bottom.min(last_row);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        }
    }

    fn in_scroll_region(&self, row: usize) -> bool {
        row >= self.scroll_top && row <= self.scroll_bottom
    }

    fn clear_all(&mut self) {
        self.cells.fill(StyledCell::default());
        self.row_boundaries.fill(RowBoundary::None);
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    fn clear_line_from_cursor(&mut self) {
        let row_start = self.cursor_y * self.width;
        for x in self.cursor_x..self.width {
            self.cells[row_start + x] = StyledCell::default();
        }
    }

    fn clear_line_to_cursor(&mut self) {
        let row_start = self.cursor_y * self.width;
        for x in 0..=self.cursor_x.min(self.width.saturating_sub(1)) {
            self.cells[row_start + x] = StyledCell::default();
        }
    }

    fn clear_entire_line(&mut self) {
        self.clear_row(self.cursor_y);
    }

    fn clear_to_end(&mut self) {
        self.clear_line_from_cursor();
        for y in (self.cursor_y + 1)..self.height {
            self.clear_row(y);
        }
    }

    fn clear_to_beginning(&mut self) {
        self.clear_line_to_cursor();
        for y in 0..self.cursor_y {
            self.clear_row(y);
        }
    }

    fn linefeed(&mut self, boundary: RowBoundary) {
        if self.cursor_y < self.row_boundaries.len() {
            self.row_boundaries[self.cursor_y] = boundary;
        }
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up_in_region(self.scroll_top, self.scroll_bottom, 1, true);
        } else {
            self.cursor_y = (self.cursor_y + 1).min(self.height.saturating_sub(1));
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_x = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_x >= self.width {
            // Pending-wrap state: just cancel the wrap, land on last column
            self.cursor_x = self.width.saturating_sub(1);
        } else {
            self.cursor_x = self.cursor_x.saturating_sub(1);
        }
    }

    fn tab(&mut self) {
        let next = ((self.cursor_x / 8) + 1) * 8;
        self.cursor_x = next.min(self.width.saturating_sub(1));
    }

    fn save_cursor(&mut self) {
        self.saved_cursor_x = self.cursor_x.min(self.width.saturating_sub(1));
        self.saved_cursor_y = self.cursor_y;
        self.saved_style = self.active_style;
    }

    fn restore_cursor(&mut self) {
        self.cursor_x = self.saved_cursor_x.min(self.width.saturating_sub(1));
        self.cursor_y = self.saved_cursor_y.min(self.height.saturating_sub(1));
        self.active_style = self.saved_style;
    }

    fn reverse_index(&mut self) {
        if self.cursor_y == self.scroll_top {
            self.scroll_down_in_region(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.cursor_y = self.cursor_y.saturating_sub(1);
        }
    }

    fn put_char(&mut self, ch: char) {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);

        // Wide char doesn't fit at end of line — pad remainder and wrap
        if ch_width == 2 && self.cursor_x + 1 >= self.width {
            if self.cursor_x < self.width {
                let idx = self.idx(self.cursor_x, self.cursor_y);
                self.cells[idx] = StyledCell::default();
            }
            self.cursor_x = 0;
            self.linefeed(RowBoundary::SoftWrap);
        }

        if self.cursor_x >= self.width {
            self.cursor_x = 0;
            self.linefeed(RowBoundary::SoftWrap);
        }

        if self.cursor_y >= self.height {
            self.scroll_up(1);
            self.cursor_y = self.height.saturating_sub(1);
        }

        // IRM: shift cells right before placing the new character
        if self.insert_mode {
            let row_start = self.cursor_y * self.width;
            let end = self.width;
            for x in (self.cursor_x..end).rev() {
                let dst = x + ch_width;
                if dst < end {
                    self.cells[row_start + dst] = self.cells[row_start + x].clone();
                }
                if x < self.cursor_x + ch_width {
                    self.cells[row_start + x] = StyledCell::default();
                }
            }
        }

        // If overwriting a continuation cell, clear the owning wide char
        let idx = self.idx(self.cursor_x, self.cursor_y);
        if self.cells[idx].ch == '\0' && self.cursor_x > 0 {
            let owner_idx = self.idx(self.cursor_x - 1, self.cursor_y);
            self.cells[owner_idx] = StyledCell::default();
        }

        // If overwriting a wide char, clear its continuation cell
        if self.cells[idx].ch != ' '
            && self.cells[idx].ch != '\0'
            && UnicodeWidthChar::width(self.cells[idx].ch).unwrap_or(1) == 2
            && self.cursor_x + 1 < self.width
        {
            let cont_idx = self.idx(self.cursor_x + 1, self.cursor_y);
            self.cells[cont_idx] = StyledCell::default();
        }

        self.cells[idx] = StyledCell {
            ch,
            style: self.active_style,
            link: self.active_link.clone(),
        };

        // Place continuation cell for wide characters
        if ch_width == 2 && self.cursor_x + 1 < self.width {
            let cont_idx = self.idx(self.cursor_x + 1, self.cursor_y);
            // If the continuation cell overwrites a wide char's continuation, fix owner
            if self.cells[cont_idx].ch == '\0' && self.cursor_x + 1 > 0 {
                // The owner is at cursor_x, which we just wrote — no fixup needed
            }
            // If the continuation cell overwrites a wide char, fix its continuation
            if self.cells[cont_idx].ch != ' '
                && self.cells[cont_idx].ch != '\0'
                && UnicodeWidthChar::width(self.cells[cont_idx].ch).unwrap_or(1) == 2
                && self.cursor_x + 2 < self.width
            {
                let next_cont = self.idx(self.cursor_x + 2, self.cursor_y);
                self.cells[next_cont] = StyledCell::default();
            }
            self.cells[cont_idx] = StyledCell {
                ch: '\0',
                style: self.active_style,
                link: self.active_link.clone(),
            };
        }

        self.cursor_x += ch_width; // May reach self.width — that's the "pending wrap" state
    }

    fn scroll_up(&mut self, count: usize) {
        if self.height == 0 {
            return;
        }
        self.scroll_up_in_region(0, self.height - 1, count, true);
    }

    fn scroll_up_in_region(
        &mut self,
        top: usize,
        bottom: usize,
        count: usize,
        record_scrollback: bool,
    ) {
        if top > bottom || bottom >= self.height {
            return;
        }

        let region_height = bottom - top + 1;
        let count = count.min(region_height);
        if count == 0 {
            return;
        }

        if record_scrollback && top == 0 {
            for y in top..(top + count) {
                self.push_scrollback_line(
                    self.trimmed_row_text(y),
                    self.row_cells(y),
                    self.row_boundary_to_next(y),
                );
            }
        }

        for offset in 0..(region_height - count) {
            let dst = top + offset;
            let src = dst + count;
            self.copy_row(dst, src);
        }

        for y in (bottom + 1 - count)..=bottom {
            self.clear_row(y);
        }
    }

    fn scroll_down_in_region(&mut self, top: usize, bottom: usize, count: usize) {
        if top > bottom || bottom >= self.height {
            return;
        }

        let region_height = bottom - top + 1;
        let count = count.min(region_height);
        if count == 0 {
            return;
        }

        for offset in (0..(region_height - count)).rev() {
            let src = top + offset;
            let dst = src + count;
            self.copy_row(dst, src);
        }

        for y in top..(top + count) {
            self.clear_row(y);
        }
    }

    fn insert_lines_at_cursor(&mut self, count: usize) {
        if !self.in_scroll_region(self.cursor_y) {
            return;
        }
        self.scroll_down_in_region(self.cursor_y, self.scroll_bottom, count);
    }

    fn delete_lines_at_cursor(&mut self, count: usize) {
        if !self.in_scroll_region(self.cursor_y) {
            return;
        }
        self.scroll_up_in_region(self.cursor_y, self.scroll_bottom, count, false);
    }

    fn scroll_up_current_region(&mut self, count: usize) {
        self.scroll_up_in_region(self.scroll_top, self.scroll_bottom, count, true);
    }

    fn scroll_down_current_region(&mut self, count: usize) {
        self.scroll_down_in_region(self.scroll_top, self.scroll_bottom, count);
    }

    pub(super) fn row_boundary_to_next(&self, row: usize) -> RowBoundary {
        self.row_boundaries
            .get(row)
            .copied()
            .unwrap_or(RowBoundary::None)
    }

    fn push_scrollback_line(
        &mut self,
        text: String,
        cells: Vec<StyledCell>,
        boundary_to_next: RowBoundary,
    ) {
        self.scrollback.push(HistoryLine {
            text,
            cells,
            boundary_to_next,
        });
        let overflow = self.scrollback.len().saturating_sub(MAX_SCROLLBACK_LINES);
        if overflow > 0 {
            self.scrollback.drain(0..overflow);
        }
    }

    fn csi_param(params: &Params, index: usize, default: usize) -> usize {
        params
            .iter()
            .nth(index)
            .and_then(|values| values.first().copied())
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    /// Like [`Self::csi_param`] but keeps explicit zeros. Needed for kitty
    /// keyboard flags where 0 is a meaningful value (and vte reports an
    /// omitted parameter as 0, matching the spec's "defaults to zero").
    fn kitty_flags_param(params: &Params) -> u8 {
        params
            .iter()
            .next()
            .and_then(|values| values.first().copied())
            .map(|value| (value & KITTY_KBD_ALL_FLAGS) as u8)
            .unwrap_or(0)
    }

    fn to_u8(value: Option<u16>) -> Option<u8> {
        value.and_then(|v| u8::try_from(v).ok())
    }

    /// Parse the color arguments of an SGR 38/48/58 extended-color
    /// introducer. `args` starts at the color mode (`5` or `2`). Colon-form
    /// direct color may carry an ITU colorspace id (`2:cs:r:g:b`), which is
    /// skipped. Returns the parsed color (if complete) and how many of
    /// `args` were consumed.
    fn parse_color_args(args: &[u16], colon_form: bool) -> (Option<Color>, usize) {
        match args.first() {
            Some(5) => {
                let color = Self::to_u8(args.get(1).copied()).map(Color::AnsiValue);
                (color, args.len().min(2))
            }
            Some(2) => {
                // Colon form with >=5 args carries a colorspace id between
                // the mode and the components; the semicolon form never does.
                let rgb_start = if colon_form && args.len() >= 5 { 2 } else { 1 };
                let color = match (
                    Self::to_u8(args.get(rgb_start).copied()),
                    Self::to_u8(args.get(rgb_start + 1).copied()),
                    Self::to_u8(args.get(rgb_start + 2).copied()),
                ) {
                    (Some(r), Some(g), Some(b)) => Some(Color::Rgb { r, g, b }),
                    _ => None,
                };
                (color, args.len().min(rgb_start + 3))
            }
            Some(_) => (None, 1),
            None => (None, 0),
        }
    }

    /// Parse the payload of an SGR 38/48/58 parameter. `group` is the
    /// parameter the introducer arrived in (colon subparameters travel in
    /// the same group), `rest` the following parameters (legacy semicolon
    /// form). Returns the color and how many *following* parameters were
    /// consumed (always 0 for the colon form).
    fn parse_sgr_color(group: &[u16], rest: &[&[u16]]) -> (Option<Color>, usize) {
        if group.len() > 1 {
            let (color, _) = Self::parse_color_args(&group[1..], true);
            (color, 0)
        } else {
            let args = rest
                .iter()
                .take(4)
                .map(|group| group.first().copied().unwrap_or(0))
                .collect::<Vec<_>>();
            Self::parse_color_args(&args, false)
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        // Keep each parameter's colon subparameters grouped: flattening them
        // into the top-level code stream desynchronizes attribute state
        // (e.g. `4:0` would enable underline via `4`, `58:5:4` would enable
        // blink and underline via its color arguments).
        let groups = params.iter().collect::<Vec<_>>();
        if groups.is_empty() {
            self.active_style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let code = group.first().copied().unwrap_or(0);
            match code {
                0 => {
                    self.active_style = CellStyle::default();
                }
                1 => self.active_style.bold = true,
                2 => self.active_style.dim = true,
                3 => self.active_style.italic = true,
                // `4` may carry an underline-style subparameter (kitty
                // extension): 4:0 disables, 4:1..=4:5 select styled
                // underlines, all rendered here as a plain underline.
                4 => {
                    self.active_style.underlined =
                        group.get(1).copied().is_none_or(|style| style != 0);
                }
                5 => self.active_style.slow_blink = true,
                6 => self.active_style.rapid_blink = true,
                7 => self.active_style.reverse = true,
                8 => self.active_style.hidden = true,
                9 => self.active_style.crossed_out = true,
                // ECMA-48 defines 21 as doubly underlined; xterm, kitty and
                // ghostty agree (bold-off is 22). Rendered as a plain
                // underline.
                21 => self.active_style.underlined = true,
                22 => {
                    self.active_style.bold = false;
                    self.active_style.dim = false;
                }
                23 => self.active_style.italic = false,
                24 => self.active_style.underlined = false,
                25 => {
                    self.active_style.slow_blink = false;
                    self.active_style.rapid_blink = false;
                }
                27 => self.active_style.reverse = false,
                28 => self.active_style.hidden = false,
                29 => self.active_style.crossed_out = false,
                30..=37 => self.active_style.fg = Some(Color::AnsiValue((code - 30) as u8)),
                39 => self.active_style.fg = None,
                40..=47 => self.active_style.bg = Some(Color::AnsiValue((code - 40) as u8)),
                49 => self.active_style.bg = None,
                90..=97 => {
                    self.active_style.fg = Some(Color::AnsiValue((code - 90 + 8) as u8));
                }
                100..=107 => {
                    self.active_style.bg = Some(Color::AnsiValue((code - 100 + 8) as u8));
                }
                38 | 48 | 58 => {
                    let (color, consumed) = Self::parse_sgr_color(group, &groups[i + 1..]);
                    match code {
                        38 => {
                            if let Some(color) = color {
                                self.active_style.fg = Some(color);
                            }
                        }
                        48 => {
                            if let Some(color) = color {
                                self.active_style.bg = Some(color);
                            }
                        }
                        // 58 (underline color) is parsed only so its
                        // arguments cannot leak into the code stream; the
                        // color itself is not tracked.
                        _ => {}
                    }
                    i += consumed + 1;
                    continue;
                }
                // 59 resets the underline color, which is not tracked.
                59 => {}
                _ => {}
            }
            i += 1;
        }
    }

    fn osc_payload_bytes(params: &[&[u8]], start: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        for (index, param) in params.iter().enumerate().skip(start) {
            if index > start {
                payload.push(b';');
            }
            payload.extend_from_slice(param);
        }
        payload
    }
}

fn parse_osc7_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("file://") {
        let path_start = rest.find('/')?;
        let path = rest[path_start..].trim_matches('\0');
        let decoded = percent_decode(path.as_bytes())?;
        return sanitize_display_text(&decoded);
    }

    sanitize_display_text(trimmed)
}

fn percent_decode(bytes: &[u8]) -> Option<String> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = bytes[index + 1] as char;
            let low = bytes[index + 2] as char;
            let hex = [high, low].iter().collect::<String>();
            let value = u8::from_str_radix(&hex, 16).ok()?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn sanitize_display_text(input: &str) -> Option<String> {
    const MAX_BYTES: usize = 256;

    let mut clean = String::new();
    for ch in input.chars() {
        if ch.is_control() {
            continue;
        }
        let ch_len = ch.len_utf8();
        if clean.len() + ch_len > MAX_BYTES {
            break;
        }
        clean.push(ch);
    }

    (!clean.is_empty()).then_some(clean)
}

impl Perform for TerminalGrid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.linefeed(RowBoundary::HardLf);
            }
            b'\r' => self.carriage_return(),
            0x08 => self.backspace(),
            b'\t' => self.tab(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(RowBoundary::None),
            b'M' => self.reverse_index(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(ps) = params.first() else {
            return;
        };

        match *ps {
            b"8" => {
                // OSC 8 is modelled per-cell (see `active_link` below) and the
                // renderer re-emits balanced hyperlink sequences aligned with the
                // frame. Forwarding the raw guest sequence to the host as well
                // double-emits and, when a guest's open and close land in
                // different output bursts, leaves the host stuck in an open
                // hyperlink state that bleeds underline/link styling across the
                // whole frame (status line included). So OSC 8 is intentionally
                // not passed through.
                //
                // OSC 8 ; params ; uri — an empty URI closes the hyperlink,
                // a non-empty one opens it for subsequently printed cells.
                let uri = Self::osc_payload_bytes(params, 2);
                self.active_link = if uri.is_empty() || uri.len() > MAX_OSC8_URI_LEN {
                    None
                } else {
                    String::from_utf8(uri).ok().map(Arc::from)
                };
            }
            b"0" | b"2" => {
                let payload = Self::osc_payload_bytes(params, 1);
                let title = if payload.is_empty() {
                    None
                } else {
                    let Ok(raw) = String::from_utf8(payload) else {
                        return;
                    };
                    sanitize_display_text(&raw)
                };
                self.terminal_events
                    .push(TerminalEvent::TitleChanged { title });
            }
            b"7" => {
                let payload = Self::osc_payload_bytes(params, 1);
                if payload.is_empty() {
                    return;
                }
                let Ok(raw) = String::from_utf8(payload) else {
                    return;
                };
                if let Some(cwd) = parse_osc7_path(&raw) {
                    self.terminal_events.push(TerminalEvent::CwdChanged { cwd });
                }
            }
            b"10" | b"11" => {
                // OSC 10/11: default foreground/background color. Only the
                // query form ("?") is answered, using the colors mirrored
                // from the most recently attached client's host terminal
                // (reported once in the Hello handshake and cached
                // server-side). When no color is cached the query stays
                // unanswered, preserving the previous behaviour (guest
                // falls back to its own timeout). Guests *setting* these
                // colors are ignored in v1: honouring a set would require
                // repainting the host terminal and restoring it on detach.
                if params.get(1).copied() != Some(b"?".as_slice()) {
                    return;
                }
                let color = if *ps == b"10" {
                    self.host_colors.fg
                } else {
                    self.host_colors.bg
                };
                let Some((r, g, b)) = color else {
                    return;
                };
                // 16-bit-per-channel reply form: each 8-bit channel byte is
                // doubled (0xab -> "abab"), matching xterm's replies. The
                // terminator mirrors the query's (BEL or ST).
                let code = if *ps == b"10" { 10 } else { 11 };
                let mut response =
                    format!("\x1b]{code};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
                        .into_bytes();
                if bell_terminated {
                    response.push(0x07);
                } else {
                    response.extend_from_slice(b"\x1b\\");
                }
                self.response_queue.push(response);
            }
            b"133" => {
                // OSC 133 semantic prompt marks (shell integration). Only
                // the prompt-start mark ("A") is tracked for now; it gives
                // downstream consumers (e.g. agent detection) the location
                // of the last shell prompt.
                if params
                    .get(1)
                    .is_some_and(|kind| kind.first() == Some(&b'A'))
                {
                    self.last_prompt_abs_row = Some(self.scrollback.len() + self.cursor_y);
                }
            }
            b"52" => {
                // OSC 52 clipboard write: params are (52, selection, base64
                // data). Queries ("?") are not answered, and payloads that
                // fail to decode (including parser-truncated ones) are
                // dropped.
                let payload: &[u8] = params.get(2).copied().unwrap_or(b"");
                if payload.is_empty() || payload == b"?" || payload.len() > MAX_OSC52_BASE64_LEN {
                    return;
                }
                use base64::Engine as _;
                let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload) else {
                    return;
                };
                let text = String::from_utf8_lossy(&decoded).into_owned();
                if text.is_empty() {
                    return;
                }
                self.terminal_events
                    .push(TerminalEvent::ClipboardSet { text });
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Clamp cursor_x (clear pending-wrap) only for cursor-movement sequences
        match action {
            'A' | 'B' | 'C' | 'D' | 'H' | 'f' | 'G' | 'd' | 'E' | 'F' | 's' | 'u' | 'J' | 'K'
            | 'X' | 'P' | '@' | 'L' | 'M' => {
                self.cursor_x = self.cursor_x.min(self.width.saturating_sub(1));
            }
            _ => {}
        }

        match action {
            'A' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(delta);
            }
            'B' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = (self.cursor_y + delta).min(self.height.saturating_sub(1));
            }
            'C' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_x = (self.cursor_x + delta).min(self.width.saturating_sub(1));
            }
            'D' => {
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_x = self.cursor_x.saturating_sub(delta);
            }
            'H' | 'f' => {
                let row = Self::csi_param(params, 0, 1);
                let col = Self::csi_param(params, 1, 1);
                self.cursor_y = row.saturating_sub(1).min(self.height.saturating_sub(1));
                self.cursor_x = col.saturating_sub(1).min(self.width.saturating_sub(1));
            }
            'J' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    1 => self.clear_to_beginning(),
                    2 => self.clear_all(),
                    3 => self.clear_scrollback(),
                    _ => self.clear_to_end(),
                }
            }
            'K' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    1 => self.clear_line_to_cursor(),
                    2 => self.clear_entire_line(),
                    _ => self.clear_line_from_cursor(),
                }
            }
            'L' => {
                let count = Self::csi_param(params, 0, 1);
                self.insert_lines_at_cursor(count);
            }
            'M' => {
                let count = Self::csi_param(params, 0, 1);
                self.delete_lines_at_cursor(count);
            }
            'S' => {
                let count = Self::csi_param(params, 0, 1);
                self.scroll_up_current_region(count);
            }
            'T' => {
                let count = Self::csi_param(params, 0, 1);
                self.scroll_down_current_region(count);
            }
            'r' => {
                let top = Self::csi_param(params, 0, 1);
                let bottom = Self::csi_param(params, 1, self.height);
                self.set_scroll_region(top.saturating_sub(1), bottom.saturating_sub(1));
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            'G' => {
                // CHA — Cursor Horizontal Absolute
                let col = Self::csi_param(params, 0, 1);
                self.cursor_x = col.saturating_sub(1).min(self.width.saturating_sub(1));
            }
            'd' => {
                // VPA — Vertical Position Absolute
                let row = Self::csi_param(params, 0, 1);
                self.cursor_y = row.saturating_sub(1).min(self.height.saturating_sub(1));
            }
            'E' => {
                // CNL — Cursor Next Line
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = (self.cursor_y + delta).min(self.height.saturating_sub(1));
                self.cursor_x = 0;
            }
            'F' => {
                // CPL — Cursor Previous Line
                let delta = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(delta);
                self.cursor_x = 0;
            }
            'X' => {
                // ECH — Erase Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                for i in 0..count {
                    let x = self.cursor_x + i;
                    if x >= self.width {
                        break;
                    }
                    self.cells[row_start + x] = StyledCell::default();
                }
            }
            'P' => {
                // DCH — Delete Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                let end = self.width;
                for x in self.cursor_x..end {
                    let src = x + count;
                    self.cells[row_start + x] = if src < end {
                        self.cells[row_start + src].clone()
                    } else {
                        StyledCell::default()
                    };
                }
            }
            '@' => {
                // ICH — Insert Character
                let count = Self::csi_param(params, 0, 1);
                let row_start = self.cursor_y * self.width;
                let end = self.width;
                for x in (self.cursor_x..end).rev() {
                    let dst = x + count;
                    if dst < end {
                        self.cells[row_start + dst] = self.cells[row_start + x].clone();
                    }
                    if x < self.cursor_x + count {
                        self.cells[row_start + x] = StyledCell::default();
                    }
                }
            }
            'h' if intermediates == [b'?'] => {
                for param in params.iter() {
                    match param[0] {
                        47 | 1047 | 1049 => self.enter_alternate_screen(),
                        9 => self.mouse_protocol = MouseProtocol::X10,
                        1000 => self.mouse_protocol = MouseProtocol::Normal,
                        1002 => self.mouse_protocol = MouseProtocol::ButtonEvent,
                        1003 => self.mouse_protocol = MouseProtocol::AnyEvent,
                        1006 => self.mouse_sgr = true,
                        2004 => self.bracketed_paste = true,
                        2026 => self.sync_output_since = Some(std::time::Instant::now()),
                        _ => {}
                    }
                }
            }
            'l' if intermediates == [b'?'] => {
                for param in params.iter() {
                    match param[0] {
                        47 | 1047 | 1049 => self.leave_alternate_screen(),
                        9 | 1000 | 1002 | 1003 => self.mouse_protocol = MouseProtocol::None,
                        1006 => self.mouse_sgr = false,
                        2004 => self.bracketed_paste = false,
                        2026 => self.sync_output_since = None,
                        _ => {}
                    }
                }
            }
            // SM — Set Mode (ANSI modes, no `?` prefix)
            'h' if intermediates.is_empty() => {
                for param in params.iter() {
                    if param[0] == 4 {
                        self.insert_mode = true;
                    }
                }
            }
            // RM — Reset Mode (ANSI modes, no `?` prefix)
            'l' if intermediates.is_empty() => {
                for param in params.iter() {
                    if param[0] == 4 {
                        self.insert_mode = false;
                    }
                }
            }
            'n' if intermediates.is_empty() => {
                // DSR — Device Status Report
                let ps = Self::csi_param(params, 0, 0);
                match ps {
                    5 => {
                        // Status report: OK
                        self.response_queue.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // Cursor position report (1-based)
                        let row = self.cursor_y + 1;
                        let col = self.cursor_x.min(self.width.saturating_sub(1)) + 1;
                        self.response_queue
                            .push(format!("\x1b[{row};{col}R").into_bytes());
                    }
                    _ => {}
                }
            }
            'n' if intermediates == [b'?'] => {
                // DECDSR — DEC private Device Status Report
                let ps = Self::csi_param(params, 0, 0);
                if ps == 6 {
                    // DEC cursor position report (1-based)
                    let row = self.cursor_y + 1;
                    let col = self.cursor_x.min(self.width.saturating_sub(1)) + 1;
                    self.response_queue
                        .push(format!("\x1b[?{row};{col}R").into_bytes());
                }
            }
            't' if intermediates.is_empty() => {
                // XTWINOPS — Window manipulation
                let ps = Self::csi_param(params, 0, 0);
                if ps == 18 {
                    // Report text area size in characters
                    self.response_queue
                        .push(format!("\x1b[8;{};{}t", self.height, self.width).into_bytes());
                }
            }
            'c' if intermediates.is_empty() || intermediates == [b'>'] => {
                // DA — Device Attributes
                // Respond as a VT220-compatible terminal
                if intermediates.is_empty() {
                    self.response_queue.push(b"\x1b[?62;22c".to_vec());
                } else {
                    // Secondary DA
                    self.response_queue.push(b"\x1b[>1;1;0c".to_vec());
                }
            }
            's' if intermediates.is_empty() => {
                self.save_cursor();
            }
            'u' if intermediates.is_empty() => {
                self.restore_cursor();
            }
            // Kitty keyboard protocol (progressive enhancement) state
            // machine. Only the flag stack lives here; the key-encoding
            // side reads the current flags via `kitty_keyboard_flags`.
            'u' if intermediates == [b'?'] => {
                // Query: reply with the flags in effect for the active screen
                // so guests can detect support.
                let flags = self.kitty_keyboard_flags();
                self.response_queue
                    .push(format!("\x1b[?{flags}u").into_bytes());
            }
            'u' if intermediates == [b'>'] => {
                // Push the given flags (omitted flags default to zero).
                let flags = Self::kitty_flags_param(params);
                self.kitty_kbd_mut().push(flags);
            }
            'u' if intermediates == [b'<'] => {
                // Pop `n` entries (default 1).
                let count = Self::csi_param(params, 0, 1);
                self.kitty_kbd_mut().pop(count);
            }
            'u' if intermediates == [b'='] => {
                // Set flags without pushing: mode 1 assigns all bits
                // (default), mode 2 sets the given bits, mode 3 clears them.
                let flags = Self::kitty_flags_param(params);
                let mode = Self::csi_param(params, 1, 1);
                self.kitty_kbd_mut().set(flags, mode);
            }
            'q' if intermediates == [b' '] => {
                // DECSCUSR — Set Cursor Style
                let ps = Self::csi_param(params, 0, 0);
                self.cursor_style = match ps {
                    0 | 1 => crossterm::cursor::SetCursorStyle::BlinkingBlock,
                    2 => crossterm::cursor::SetCursorStyle::SteadyBlock,
                    3 => crossterm::cursor::SetCursorStyle::BlinkingUnderScore,
                    4 => crossterm::cursor::SetCursorStyle::SteadyUnderScore,
                    5 => crossterm::cursor::SetCursorStyle::BlinkingBar,
                    6 => crossterm::cursor::SetCursorStyle::SteadyBar,
                    _ => crossterm::cursor::SetCursorStyle::DefaultUserShape,
                };
            }
            // SGR — attributes only when no private marker is present.
            // xterm reuses the `m` final byte with markers for keyboard
            // protocol controls: XTMODKEYS `CSI > Pp;Pv m` (sent by Claude
            // Code as `CSI > 4;2 m`, which would otherwise read as SGR 4;2 =
            // underline + dim and stick for the whole frame) and XTQMODKEYS
            // `CSI ? Pp m`. Those must not touch the attribute state.
            'm' if intermediates.is_empty() => {
                self.apply_sgr(params);
            }
            _ => {}
        }
    }
}
