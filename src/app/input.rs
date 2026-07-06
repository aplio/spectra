use super::*;

/// Clicks this close in time continue a multi-click selection chain.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);
/// Clicks may drift this many cells from the previous click and still
/// continue the chain (double-clicks rarely land on the exact same cell).
const MULTI_CLICK_RADIUS_CELLS: usize = 1;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        if self.current_session_mut().reset_focused_pane_view_scroll() {
            self.needs_render = true;
        }

        if self.view.locked_input {
            if (matches!(key.code, crossterm::event::KeyCode::Esc) && key.modifiers.is_empty())
                || self.view.keys.check_global_action(key) == Some(CommandAction::LeaveLockMode)
            {
                self.view.locked_input = false;
                self.set_message("lock mode off", Duration::from_secs(2));
                self.needs_render = true;
                return Ok(AppSignal::None);
            }
            return match self
                .kitty_encode_for_focused(key)
                .or_else(|| encode_key_to_bytes(key))
            {
                Some(bytes) => self.handle_send_bytes(bytes),
                None => Ok(AppSignal::None),
            };
        }

        if !matches!(self.view.input_mode, InputMode::Normal) {
            self.needs_render = true;
            return self.handle_mode_key(key);
        }

        let prefix_active_before = self.view.keys.prefix_active();
        match self.view.keys.handle_key(key) {
            InputAction::Command(action) => {
                // A globally-bound copy (cmd+c) only acts on an existing
                // mouse selection; without one the key keeps its normal
                // meaning for the pane, mirroring how terminals treat their
                // own copy shortcut as "performable".
                if action == CommandAction::CopySelection
                    && !prefix_active_before
                    && self.view.text_selection.is_none()
                {
                    return match self
                        .kitty_encode_for_focused(key)
                        .or_else(|| encode_key_to_bytes(key))
                    {
                        Some(bytes) => self.handle_send_bytes(bytes),
                        None => Ok(AppSignal::None),
                    };
                }
                self.needs_render = true;
                Ok(self.handle_action(action))
            }
            InputAction::SendBytes(bytes) => {
                if self.view.keys.prefix_active() != prefix_active_before {
                    self.needs_render = true;
                }
                // Typing invalidates a lingering mouse selection: the pane
                // content is about to change under the highlight.
                if self.view.text_selection.take().is_some() {
                    self.view.click_chain = None;
                    self.needs_render = true;
                }
                // Keys destined for the pane are re-encoded in kitty form
                // when its guest enabled the kitty keyboard protocol.
                let bytes = self.kitty_encode_for_focused(key).unwrap_or(bytes);
                self.handle_send_bytes(bytes)
            }
            InputAction::Ignore => {
                if self.view.keys.prefix_active() != prefix_active_before {
                    self.needs_render = true;
                }
                Ok(AppSignal::None)
            }
        }
    }

    fn handle_send_bytes(&mut self, bytes: Vec<u8>) -> io::Result<AppSignal> {
        let ctrl_d = bytes.as_slice() == [0x04];
        match self.send_input_to_active_window(&bytes) {
            Ok(()) => {
                if ctrl_d && self.current_session_mut().focused_pane_closed() {
                    self.close_focused_or_quit("pane process exited");
                }
                Ok(AppSignal::None)
            }
            Err(err) if is_closed_pane_error(&err) => {
                self.close_focused_or_quit("write to closed pane");
                Ok(AppSignal::None)
            }
            Err(err) if ctrl_d => {
                self.set_message(
                    &format!("ctrl+d write failed: {err}"),
                    Duration::from_secs(3),
                );
                self.write_log(&format!("ctrl+d write failed: {err}"));
                Ok(AppSignal::None)
            }
            Err(err) => Err(err),
        }
    }

    /// Encode `key` in kitty CSI-u form when the focused pane's guest
    /// enabled the kitty keyboard protocol with a flag we implement (bit 1
    /// disambiguate, bit 8 report-all). `None` means "use the legacy
    /// encoding". With pane synchronization active this still keys off the
    /// focused pane (v1 simplification).
    fn kitty_encode_for_focused(&self, key: KeyEvent) -> Option<Vec<u8>> {
        let flags = self.current_session().focused_kitty_keyboard_flags();
        if flags & (KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_ALL) == 0 {
            return None;
        }
        encode_key_to_bytes_kitty(key, flags)
    }

    fn send_input_to_active_window(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.current_session().active_window_synchronize_panes() {
            let _ = self.current_session_mut().send_to_active_window(bytes)?;
            Ok(())
        } else {
            self.current_session_mut().send_to_focused(bytes)
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.view.locked_input {
            self.view.mouse_drag = None;
            return;
        }

        // Forward mouse input to the guest program when the pane under the
        // cursor requested mouse reporting (DECSET 9/1000/1002/1003). This
        // works regardless of spectra's own [mouse] config. Shift bypasses
        // forwarding (the conventional escape hatch for host-side handling),
        // and an in-flight spectra drag/selection keeps priority. A completed
        // selection (button released) does not block forwarding; it is
        // dropped once the guest consumes a fresh press.
        if matches!(self.view.input_mode, InputMode::Normal)
            && !mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT)
            && self.view.mouse_drag.is_none()
            && !self
                .view
                .text_selection
                .is_some_and(|selection| selection.dragging)
            && self.forward_mouse_to_guest(&mouse)
        {
            if matches!(mouse.kind, MouseEventKind::Down(_))
                && self.view.text_selection.take().is_some()
            {
                self.view.click_chain = None;
                self.needs_render = true;
            }
            // A left-drag consumed by the guest is almost always a user
            // trying to select text; surface the shift bypass. Re-arming the
            // message on every drag event keeps it visible while dragging.
            // Skip it when spectra's own mouse handling is off (shift+drag
            // would not select anything either).
            if self.mouse_enabled && matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
                self.set_message("shift+drag to select text", Duration::from_secs(2));
                self.needs_render = true;
            }
            return;
        }

        if !self.mouse_enabled {
            self.view.mouse_drag = None;
            return;
        }

        // Handle scroll events in both Normal and cursor modes.
        const MOUSE_SCROLL_LINES: isize = 3;
        let pane_view_rows = usize::from(self.view.rows.saturating_sub(1)).max(1);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.needs_render = true;
                if let InputMode::CursorMode { ref mut state } = self.view.input_mode {
                    Self::cursor_mode_scroll_by(state, -MOUSE_SCROLL_LINES, pane_view_rows);
                } else {
                    self.current_session_mut()
                        .scroll_focused_pane(MOUSE_SCROLL_LINES, pane_view_rows);
                }
                return;
            }
            MouseEventKind::ScrollDown => {
                self.needs_render = true;
                if let InputMode::CursorMode { ref mut state } = self.view.input_mode {
                    Self::cursor_mode_scroll_by(state, MOUSE_SCROLL_LINES, pane_view_rows);
                } else {
                    self.current_session_mut()
                        .scroll_focused_pane(-MOUSE_SCROLL_LINES, pane_view_rows);
                }
                return;
            }
            _ => {}
        }

        if !matches!(self.view.input_mode, InputMode::Normal) {
            self.view.mouse_drag = None;
            return;
        }

        self.needs_render = true;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let prev_chain = self.view.click_chain.take();
                self.view.mouse_drag = None;
                self.view.text_selection = None;
                let side_window_tree = self.side_window_tree_overlay();
                if let Some(side) = side_window_tree.as_ref()
                    && let Some((session_index, window_number)) =
                        self.side_window_tree_target_at(side, mouse.column, mouse.row)
                {
                    if session_index != self.view.active_session {
                        self.select_session(session_index);
                    }
                    if self
                        .current_session_mut()
                        .focus_window_number(window_number)
                        .is_ok()
                    {
                        self.record_focus_for_active_session();
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    return;
                }
                let frame =
                    self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
                if let Some(divider) = Self::mouse_divider_at(&frame, mouse.column, mouse.row)
                    && let Some(pane_id) = Self::mouse_anchor_pane_for_divider(&frame, divider)
                {
                    if self.current_session_mut().focus_pane_id(pane_id).is_ok() {
                        self.record_focus_for_active_session();
                        self.view.mouse_drag = Some(MouseDragState {
                            pane_id,
                            orientation: divider.orientation,
                            last_col: mouse.column,
                            last_row: mouse.row,
                        });
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    return;
                }

                if let Some(pane) = Self::mouse_pane_info_at(&frame, mouse.column, mouse.row) {
                    if self
                        .current_session_mut()
                        .focus_pane_id(pane.pane_id)
                        .is_ok()
                    {
                        self.record_focus_for_active_session();
                        self.persist_active_session_info();
                        self.needs_full_clear = true;
                    }
                    let local_col = usize::from(mouse.column)
                        .saturating_sub(pane.rect.x)
                        .min(pane.rect.width.saturating_sub(1));
                    let local_row = usize::from(mouse.row)
                        .saturating_sub(pane.rect.y)
                        .min(pane.rect.height.saturating_sub(1));
                    let absolute_row = pane.view_row_origin.saturating_add(local_row);
                    let pane_id = pane.pane_id;
                    let (pane_x, pane_y) = (pane.rect.x, pane.rect.y);
                    let (pane_width, pane_height) = (pane.rect.width, pane.rect.height);

                    let now = Instant::now();
                    // A rapid click near the chain origin expands the selection
                    // to the next larger unit (word -> WORD -> line), gargo-style.
                    let continued_chain = prev_chain.filter(|chain| {
                        chain.pane_id == pane_id
                            && chain.origin_abs_row == absolute_row
                            && now.duration_since(chain.last_click_time) <= MULTI_CLICK_WINDOW
                            && local_col.abs_diff(chain.last_col) <= MULTI_CLICK_RADIUS_CELLS
                    });

                    if let Some(chain) = continued_chain {
                        let cells = self
                            .current_session()
                            .pane_absolute_row_cells(pane_id, chain.origin_abs_row)
                            .unwrap_or_default();
                        let range = Self::expand_click_selection(
                            &cells,
                            chain.origin_col,
                            chain.last_range,
                        )
                        // Nothing larger to grow into: keep the current
                        // selection instead of collapsing it.
                        .or(chain.last_range);
                        if let Some((start_col, end_col)) = range {
                            self.view.text_selection = Some(TextSelectionState {
                                pane_id,
                                start_col,
                                start_abs_row: chain.origin_abs_row,
                                end_col,
                                end_abs_row: chain.origin_abs_row,
                                pane_x,
                                pane_y,
                                pane_width,
                                pane_height,
                                dragging: true,
                            });
                        }
                        self.view.click_chain = Some(ClickChainState {
                            last_range: range,
                            last_click_time: now,
                            last_col: local_col,
                            ..chain
                        });
                        return;
                    }

                    self.view.text_selection = Some(TextSelectionState {
                        pane_id,
                        start_col: local_col,
                        start_abs_row: absolute_row,
                        end_col: local_col,
                        end_abs_row: absolute_row,
                        pane_x,
                        pane_y,
                        pane_width,
                        pane_height,
                        dragging: true,
                    });
                    self.view.click_chain = Some(ClickChainState {
                        pane_id,
                        origin_col: local_col,
                        origin_abs_row: absolute_row,
                        last_range: None,
                        last_click_time: now,
                        last_col: local_col,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Text selection drag takes priority over divider drag
                if let Some(mut sel) = self.view.text_selection
                    && sel.dragging
                {
                    let side_window_tree = self.side_window_tree_overlay();
                    let frame =
                        self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
                    let Some(pane) = frame.panes.iter().find(|pane| pane.pane_id == sel.pane_id)
                    else {
                        self.view.text_selection = None;
                        return;
                    };
                    sel.pane_x = pane.rect.x;
                    sel.pane_y = pane.rect.y;
                    sel.pane_width = pane.rect.width;
                    sel.pane_height = pane.rect.height;
                    let col = usize::from(mouse.column)
                        .saturating_sub(sel.pane_x)
                        .min(sel.pane_width.saturating_sub(1));
                    let row = usize::from(mouse.row)
                        .saturating_sub(sel.pane_y)
                        .min(sel.pane_height.saturating_sub(1));
                    let abs_row = pane.view_row_origin.saturating_add(row);
                    // Ignore drag jitter inside the multi-click origin cell or
                    // the expanded (word/line) range so a slightly wobbly
                    // double-click keeps its selection. Dragging beyond it
                    // leaves the chain: the pointer takes over, extending from
                    // the expanded start, and the next click starts fresh.
                    if let Some(chain) = self.view.click_chain {
                        let in_origin_cell =
                            col == chain.origin_col && abs_row == chain.origin_abs_row;
                        let in_expanded_range = chain.last_range.is_some_and(|(start, end)| {
                            abs_row == chain.origin_abs_row && col >= start && col <= end
                        });
                        if in_origin_cell || in_expanded_range {
                            return;
                        }
                        self.view.click_chain = None;
                    }
                    sel.end_col = col;
                    sel.end_abs_row = abs_row;
                    self.view.text_selection = Some(sel);
                    return;
                }

                let Some(mut drag) = self.view.mouse_drag else {
                    return;
                };

                let delta_col = i32::from(mouse.column) - i32::from(drag.last_col);
                let delta_row = i32::from(mouse.row) - i32::from(drag.last_row);
                let (direction, amount) = match drag.orientation {
                    crate::ui::window_manager::DividerOrientation::Vertical => {
                        if delta_col == 0 {
                            return;
                        }
                        if delta_col > 0 {
                            (
                                crate::ui::window_manager::Direction::Right,
                                delta_col as u16,
                            )
                        } else {
                            (
                                crate::ui::window_manager::Direction::Left,
                                (-delta_col) as u16,
                            )
                        }
                    }
                    crate::ui::window_manager::DividerOrientation::Horizontal => {
                        if delta_row == 0 {
                            return;
                        }
                        if delta_row > 0 {
                            (crate::ui::window_manager::Direction::Down, delta_row as u16)
                        } else {
                            (
                                crate::ui::window_manager::Direction::Up,
                                (-delta_row) as u16,
                            )
                        }
                    }
                };

                let resized = {
                    let (cols, rows) = self.current_effective_pane_dims();
                    let session = self.current_session_mut();
                    if session.focus_pane_id(drag.pane_id).is_ok() {
                        session.resize_focused(direction, amount, cols, rows)
                    } else {
                        Err("mouse resize pane missing".to_string())
                    }
                };

                if resized.is_ok() {
                    drag.last_col = mouse.column;
                    drag.last_row = mouse.row;
                    self.view.mouse_drag = Some(drag);
                    self.needs_full_clear = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.view.mouse_drag = None;
                // Releasing the button completes the gesture but keeps the
                // selection visible (copy it with the copy-selection binding).
                // A plain click that never grew a range selects nothing; a
                // multi-click selection stays even when it is a single cell.
                if let Some(mut sel) = self.view.text_selection.take() {
                    sel.dragging = false;
                    let has_range =
                        sel.start_col != sel.end_col || sel.start_abs_row != sel.end_abs_row;
                    let from_click_chain = self
                        .view
                        .click_chain
                        .is_some_and(|chain| chain.last_range.is_some());
                    if has_range || from_click_chain {
                        self.view.text_selection = Some(sel);
                    }
                }
            }
            _ => {}
        }
    }

    /// Try to deliver a mouse event to the guest program in the pane under
    /// the cursor. Returns `true` when the event was consumed (delivered, or
    /// intentionally swallowed because the guest owns mouse interaction).
    fn forward_mouse_to_guest(&mut self, mouse: &MouseEvent) -> bool {
        let side_window_tree = self.side_window_tree_overlay();
        let frame = self.pane_frame_for_current_view_with_sidebar(side_window_tree.as_ref());
        let Some(pane) = Self::mouse_pane_info_at(&frame, mouse.column, mouse.row) else {
            return false;
        };
        let pane_id = pane.pane_id;
        let (rect_x, rect_y) = (pane.rect.x, pane.rect.y);
        let (rect_w, rect_h) = (pane.rect.width, pane.rect.height);
        if !self.current_session().pane_wants_mouse_reporting(pane_id) {
            return false;
        }

        let local_col = usize::from(mouse.column)
            .saturating_sub(rect_x)
            .min(rect_w.saturating_sub(1));
        let local_row = usize::from(mouse.row)
            .saturating_sub(rect_y)
            .min(rect_h.saturating_sub(1));
        let report = self.current_session().pane_mouse_report(
            pane_id,
            mouse.kind,
            mouse.modifiers,
            local_col,
            local_row,
        );
        let Some(report) = report else {
            // The guest owns mouse interaction but did not ask for this
            // event kind. Swallow motion/drag/release so they don't fall
            // back to spectra's selection handling; let presses and scroll
            // fall through (e.g. view scroll over an X10-only guest).
            return matches!(
                mouse.kind,
                MouseEventKind::Drag(_) | MouseEventKind::Moved | MouseEventKind::Up(_)
            );
        };

        if matches!(mouse.kind, MouseEventKind::Down(_))
            && self.current_session().focused_pane_id() != Some(pane_id)
            && self.current_session_mut().focus_pane_id(pane_id).is_ok()
        {
            self.record_focus_for_active_session();
            self.persist_active_session_info();
            self.needs_render = true;
        }
        let _ = self.current_session_mut().send_to_pane(pane_id, &report);
        true
    }

    fn mouse_pane_info_at(
        frame: &crate::session::manager::RenderFrame,
        col: u16,
        row: u16,
    ) -> Option<&crate::session::manager::RenderPane> {
        let col = usize::from(col);
        let row = usize::from(row);
        frame.panes.iter().find(|pane| {
            let inside_x = col >= pane.rect.x && col < pane.rect.x + pane.rect.width;
            let inside_y = row >= pane.rect.y && row < pane.rect.y + pane.rect.height;
            inside_x && inside_y
        })
    }

    fn mouse_divider_at(
        frame: &crate::session::manager::RenderFrame,
        col: u16,
        row: u16,
    ) -> Option<crate::ui::window_manager::Divider> {
        let col = usize::from(col);
        let row = usize::from(row);
        frame
            .dividers
            .iter()
            .copied()
            .find(|divider| match divider.orientation {
                crate::ui::window_manager::DividerOrientation::Vertical => {
                    col == divider.x && row >= divider.y && row < divider.y + divider.len
                }
                crate::ui::window_manager::DividerOrientation::Horizontal => {
                    row == divider.y && col >= divider.x && col < divider.x + divider.len
                }
            })
    }

    fn mouse_anchor_pane_for_divider(
        frame: &crate::session::manager::RenderFrame,
        divider: crate::ui::window_manager::Divider,
    ) -> Option<usize> {
        match divider.orientation {
            crate::ui::window_manager::DividerOrientation::Vertical => frame
                .panes
                .iter()
                .find(|pane| pane.rect.x + pane.rect.width == divider.x)
                .or_else(|| frame.panes.iter().find(|pane| pane.rect.x == divider.x + 1))
                .map(|pane| pane.pane_id),
            crate::ui::window_manager::DividerOrientation::Horizontal => frame
                .panes
                .iter()
                .find(|pane| pane.rect.y + pane.rect.height == divider.y)
                .or_else(|| frame.panes.iter().find(|pane| pane.rect.y == divider.y + 1))
                .map(|pane| pane.pane_id),
        }
    }

    /// Word class of the cell at `col`, for multi-click word selection.
    /// Wide-char continuation cells (`'\0'`) inherit their owner's class so
    /// runs never split in the middle of a wide character.
    fn click_cell_class(
        cells: &[crate::session::terminal_state::StyledCell],
        col: usize,
    ) -> super::copy_mode::CursorModeWordClass {
        let mut index = col;
        while index > 0 && cells.get(index).is_some_and(|cell| cell.ch == '\0') {
            index -= 1;
        }
        match cells.get(index) {
            Some(cell) => Self::cursor_mode_word_class(cell.ch),
            None => super::copy_mode::CursorModeWordClass::Whitespace,
        }
    }

    /// Column run (inclusive) around `origin` of cells sharing `matches`.
    fn click_class_run(
        cells: &[crate::session::terminal_state::StyledCell],
        origin: usize,
        matches: impl Fn(super::copy_mode::CursorModeWordClass) -> bool,
    ) -> (usize, usize) {
        let mut start = origin;
        while start > 0 && matches(Self::click_cell_class(cells, start - 1)) {
            start -= 1;
        }
        let mut end = origin;
        while end + 1 < cells.len() && matches(Self::click_cell_class(cells, end + 1)) {
            end += 1;
        }
        (start, end)
    }

    /// Pick the next selection step for a multi-click chain, gargo-style:
    /// candidates are every unit we can derive at `origin` (word-class run,
    /// non-whitespace run, whole line), filtered to those containing `origin`
    /// and strictly containing `current`, smallest first. Successive clicks
    /// climb word -> WORD -> line. Returns `None` when nothing larger exists.
    pub(super) fn expand_click_selection(
        cells: &[crate::session::terminal_state::StyledCell],
        origin: usize,
        current: Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        use super::copy_mode::CursorModeWordClass;

        if cells.is_empty() {
            return None;
        }
        let origin = origin.min(cells.len() - 1);
        let origin_class = Self::click_cell_class(cells, origin);

        let mut candidates: Vec<(usize, usize)> = Vec::new();
        candidates.push(Self::click_class_run(cells, origin, |class| {
            class == origin_class
        }));
        if origin_class != CursorModeWordClass::Whitespace {
            candidates.push(Self::click_class_run(cells, origin, |class| {
                class != CursorModeWordClass::Whitespace
            }));
        }
        // Whole line, trimmed of trailing blanks but always containing origin.
        let line_end = cells
            .iter()
            .rposition(|cell| cell.ch != ' ')
            .unwrap_or(0)
            .max(origin);
        candidates.push((0, line_end));

        let current_size = current.map(|(start, end)| end - start + 1).unwrap_or(0);
        candidates
            .into_iter()
            .filter(|(start, end)| *start <= origin && *end >= origin)
            .filter(|(start, end)| end - start + 1 > current_size)
            .filter(|(start, end)| match current {
                Some((cur_start, cur_end)) => *start <= cur_start && *end >= cur_end,
                None => true,
            })
            .min_by_key(|(start, end)| end - start)
    }

    /// Copy the active mouse selection (kept visible after mouse-up) to the
    /// clipboard and drop the highlight. Bound to the copy-selection action.
    pub(super) fn copy_active_text_selection(&mut self) {
        let Some(sel) = self.view.text_selection.take() else {
            self.set_message("no selection to copy", Duration::from_secs(2));
            return;
        };
        self.view.click_chain = None;
        self.copy_text_selection(&sel);
        self.needs_render = true;
    }

    fn copy_text_selection(&mut self, sel: &TextSelectionState) {
        let text = {
            let session = self.current_session();
            let Some(total_lines) = session.pane_total_lines(sel.pane_id) else {
                return;
            };
            if total_lines == 0 {
                return;
            }

            let (mut start_abs_row, start_col, mut end_abs_row, end_col) = if sel.start_abs_row
                < sel.end_abs_row
                || (sel.start_abs_row == sel.end_abs_row && sel.start_col <= sel.end_col)
            {
                (
                    sel.start_abs_row,
                    sel.start_col,
                    sel.end_abs_row,
                    sel.end_col,
                )
            } else {
                (
                    sel.end_abs_row,
                    sel.end_col,
                    sel.start_abs_row,
                    sel.start_col,
                )
            };
            let last_row = total_lines.saturating_sub(1);
            start_abs_row = start_abs_row.min(last_row);
            end_abs_row = end_abs_row.min(last_row);

            let mut lines = Vec::new();
            for abs_row in start_abs_row..=end_abs_row {
                let cells = session
                    .pane_absolute_row_cells(sel.pane_id, abs_row)
                    .unwrap_or_default();
                let from = if abs_row == start_abs_row {
                    start_col
                } else {
                    0
                };
                let to = if abs_row == end_abs_row {
                    (end_col + 1).min(cells.len())
                } else {
                    cells.len()
                };
                let text: String = cells
                    .get(from..to)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|cell| cell.ch != '\0')
                    .map(|cell| cell.ch)
                    .collect();
                lines.push(text.trim_end().to_string());
            }
            lines.join("\n")
        };

        if text.trim().is_empty() {
            return;
        }
        match self.copy_text_for_active_client(&text) {
            Ok(()) => self.set_message("copied selection", Duration::from_secs(2)),
            Err(err) => self.set_message(&format!("copy failed: {err}"), Duration::from_secs(3)),
        }
    }

    pub(super) fn copy_text_for_active_client(&mut self, text: &str) -> Result<(), String> {
        if self.active_client_id == LOCAL_CLIENT_ID {
            return crate::clipboard::copy_text(text);
        }
        self.view
            .pending_clipboard_ansi
            .push(crate::clipboard::osc52_sequence(text));
        Ok(())
    }

    fn handle_mode_key(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        let mut signal = AppSignal::None;
        let mode = std::mem::replace(&mut self.view.input_mode, InputMode::Normal);
        self.view.input_mode = match mode {
            InputMode::RenameTreeItem { .. }
            | InputMode::ConfirmDelete { .. }
            | InputMode::SystemTree { .. } => self.handle_system_tree_mode_key(mode, key),
            InputMode::CursorMode { state } => self.handle_cursor_mode_key(state, key),
            InputMode::CommandPalette { state } => {
                self.handle_command_palette_mode_key(state, key, &mut signal)
            }
            InputMode::PeekAllWindows { state } => self.handle_peek_all_windows_mode_key(state),
            InputMode::Keybindings { state } => self.handle_keybindings_mode_key(state, key),
            InputMode::Normal => InputMode::Normal,
        };
        Ok(signal)
    }

    pub(super) fn open_peek_all_windows(&mut self) {
        let state = PeekAllWindowsState {
            session_id: self.current_session_id().to_string(),
            focused_window_number: self.current_session().focused_window_number(),
            focused_pane_id: self.current_session().focused_pane_id(),
        };
        self.view.input_mode = InputMode::PeekAllWindows { state };
        self.needs_full_clear = true;
    }

    fn handle_peek_all_windows_mode_key(&mut self, state: PeekAllWindowsState) -> InputMode {
        self.restore_peek_all_windows_focus(state);
        InputMode::Normal
    }

    fn restore_peek_all_windows_focus(&mut self, state: PeekAllWindowsState) {
        let Some(session_index) = self.session_index_for_id(&state.session_id) else {
            return;
        };

        self.view.active_session = session_index;
        let mut focused = false;

        if let Some(pane_id) = state.focused_pane_id {
            focused = self.current_session_mut().focus_pane_id(pane_id).is_ok();
        }

        if !focused && let Some(window_number) = state.focused_window_number {
            focused = self
                .current_session_mut()
                .focus_window_number(window_number)
                .is_ok();
        }

        if focused {
            self.record_focus_for_active_session();
        } else {
            self.restore_focus_for_active_session_from_history();
        }
        self.persist_active_session_info();
        self.needs_full_clear = true;
    }

    pub(super) fn handle_paste(&mut self, text: String) -> io::Result<AppSignal> {
        self.needs_render = true;
        let palette_ctx = self.command_palette_context();
        match &mut self.view.input_mode {
            InputMode::RenameTreeItem { buffer, .. } => {
                buffer.push_str(&text);
                Ok(AppSignal::None)
            }
            InputMode::SystemTree { .. } | InputMode::ConfirmDelete { .. } => Ok(AppSignal::None),
            InputMode::CursorMode { .. } => Ok(AppSignal::None),
            InputMode::CommandPalette { state } => {
                let text = text.trim_end_matches(['\r', '\n']).trim_end_matches('\0');
                if state.text_input.insert_text(text) {
                    let entries = Self::command_palette_entries_for(palette_ctx);
                    let recent_command_ids = self.command_history.get_recent_commands(100);
                    let candidates =
                        Self::command_palette_candidates(state, &entries, &recent_command_ids);
                    Self::command_palette_clamp_selected(state, candidates.len());
                }
                Ok(AppSignal::None)
            }
            InputMode::PeekAllWindows { .. } => Ok(AppSignal::None),
            InputMode::Keybindings { state } => {
                if state.query_active {
                    let text = text.trim_end_matches(['\r', '\n']).trim_end_matches('\0');
                    if state.query_input.insert_text(text) {
                        let count = Self::keybinding_candidates(state).len();
                        if count == 0 {
                            state.selected = 0;
                        } else if state.selected >= count {
                            state.selected = count - 1;
                        }
                    }
                }
                Ok(AppSignal::None)
            }
            InputMode::Normal => {
                if self.current_session().active_window_synchronize_panes() {
                    // Panes in a synchronized window can disagree on
                    // bracketed paste; wrap per pane instead of fanning out
                    // one encoding keyed off the focused pane.
                    let _ = self
                        .current_session_mut()
                        .send_paste_to_active_window(&text)?;
                } else if self.current_session().focused_bracketed_paste() {
                    let bytes = crate::session::manager::bracketed_paste_bytes(&text);
                    self.send_input_to_active_window(&bytes)?;
                } else {
                    self.send_input_to_active_window(text.as_bytes())?;
                }
                Ok(AppSignal::None)
            }
        }
    }
}
