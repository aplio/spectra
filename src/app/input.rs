use super::*;

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
                self.needs_render = true;
                Ok(self.handle_action(action))
            }
            InputAction::SendBytes(bytes) => {
                if self.view.keys.prefix_active() != prefix_active_before {
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
        // and an in-flight spectra drag/selection keeps priority.
        if matches!(self.view.input_mode, InputMode::Normal)
            && !mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT)
            && self.view.mouse_drag.is_none()
            && self.view.text_selection.is_none()
            && self.forward_mouse_to_guest(&mouse)
        {
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
                self.view.mouse_drag = None;
                self.view.text_selection = None;
                let side_window_tree = self.side_window_tree_overlay();
                if let Some(side) = side_window_tree.as_ref()
                    && let Some(window_number) =
                        self.side_window_tree_window_number_at(side, mouse.column, mouse.row)
                {
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
                    self.view.text_selection = Some(TextSelectionState {
                        pane_id: pane.pane_id,
                        start_col: local_col,
                        start_abs_row: absolute_row,
                        end_col: local_col,
                        end_abs_row: absolute_row,
                        pane_x: pane.rect.x,
                        pane_y: pane.rect.y,
                        pane_width: pane.rect.width,
                        pane_height: pane.rect.height,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Text selection drag takes priority over divider drag
                if let Some(mut sel) = self.view.text_selection {
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
                    sel.end_col = col;
                    sel.end_abs_row = pane.view_row_origin.saturating_add(row);
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
                if let Some(sel) = self.view.text_selection.take()
                    && (sel.start_col != sel.end_col || sel.start_abs_row != sel.end_abs_row)
                {
                    self.copy_text_selection(&sel);
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
            InputMode::Normal => {
                if self.current_session().focused_bracketed_paste() {
                    // Strip any embedded end marker so pasted content cannot
                    // break out of the bracketed-paste region.
                    let sanitized = text.replace("\x1b[201~", "");
                    let mut bytes = Vec::with_capacity(sanitized.len() + 12);
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(sanitized.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    self.send_input_to_active_window(&bytes)?;
                } else {
                    self.send_input_to_active_window(text.as_bytes())?;
                }
                Ok(AppSignal::None)
            }
        }
    }
}
