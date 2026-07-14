use super::*;

/// How long a `prefix q` quit confirmation stays armed before it lapses.
const QUIT_CONFIRM_TTL: Duration = Duration::from_secs(3);

impl App {
    pub(super) fn kill_session_by_index(&mut self, session_index: usize) -> Result<bool, String> {
        if session_index >= self.sessions.len() {
            return Err("session index out of range".to_string());
        }

        if self.sessions.len() == 1 {
            let context = self.current_hook_context();
            self.write_log("killed final session; shutting down");
            self.emit_hook(HookEvent::SessionKilled, context);
            self.should_quit = true;
            self.needs_render = true;
            return Ok(true);
        }

        let removed = self.sessions.remove(session_index);
        let removed_context = HookContext {
            session_id: Some(removed.session_id.clone()),
            session_name: Some(removed.session.session_name().to_string()),
            ..HookContext::default()
        };
        if self.view.active_session == session_index {
            if session_index >= self.sessions.len() {
                self.view.active_session = self.sessions.len().saturating_sub(1);
            }
        } else if session_index < self.view.active_session {
            self.view.active_session -= 1;
        }
        self.view
            .pane_histories_by_session
            .remove(&removed.session_id);
        let max_session_index = self.sessions.len().saturating_sub(1);
        for state in self.inactive_client_states.values_mut() {
            if session_index < state.active_session {
                state.active_session -= 1;
            } else if state.active_session > max_session_index {
                state.active_session = max_session_index;
            }
            state.pane_histories_by_session.remove(&removed.session_id);
        }
        for profile in self.client_focus_profiles.values_mut() {
            profile
                .pane_histories_by_session
                .remove(&removed.session_id);
            if profile.active_session_id.as_deref() == Some(removed.session_id.as_str()) {
                profile.active_session_id = None;
            }
        }

        self.restore_focus_for_active_session_from_history();

        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::SessionKilled, removed_context);
        self.write_log(&format!("killed session {}", removed.session_id));
        Ok(false)
    }

    /// Handle a `prefix q` press. The first press arms a confirmation shown
    /// in the status bar; a second press within [`QUIT_CONFIRM_TTL`] actually
    /// quits. Any other key, or letting the timeout lapse, cancels it.
    pub(super) fn request_quit(&mut self) {
        let now = Instant::now();
        match self.quit_confirm_deadline {
            Some(deadline) if now <= deadline => {
                self.quit_confirm_deadline = None;
                self.should_quit = true;
            }
            _ => {
                // With a sticky prefix the mode is still active, so the
                // confirmation is just `q`; otherwise the whole chord repeats.
                let confirm_keys = if self.view.keys.prefix_sticky() {
                    "q".to_string()
                } else {
                    format!("{} q", self.view.keys.prefix_key_display())
                };
                self.quit_confirm_deadline = Some(now + QUIT_CONFIRM_TTL);
                self.set_message(
                    &format!("press {confirm_keys} again to quit"),
                    QUIT_CONFIRM_TTL,
                );
            }
        }
    }

    /// Clear a pending quit confirmation and its status-bar prompt.
    pub(super) fn cancel_quit_confirm(&mut self) {
        if self.quit_confirm_deadline.take().is_none() {
            return;
        }
        // Drop the lingering prompt so a cancelling action that sets no
        // message of its own doesn't leave "… again to quit" on screen.
        let is_prompt = self
            .view
            .status_message
            .as_ref()
            .is_some_and(|message| message.text.ends_with("q again to quit"));
        if is_prompt {
            self.view.status_message = None;
            self.needs_render = true;
        }
    }

    pub(super) fn close_focused_or_quit(&mut self, reason: &str) {
        let (cols, rows) = self.current_effective_pane_dims();

        if self.current_session().pane_count() <= 1 {
            if self.sessions.len() > 1 {
                let closed_session = self.current_session_id().to_string();
                match self.kill_session_by_index(self.view.active_session) {
                    Ok(false) => {
                        self.write_log(&format!(
                            "{reason}: final pane closed, switched from session {closed_session}"
                        ));
                        self.set_message("session closed", Duration::from_secs(2));
                    }
                    Ok(true) => {
                        self.write_log(&format!("{reason}: final pane closed, quitting"));
                        self.should_quit = true;
                    }
                    Err(err) => {
                        self.write_log(&format!(
                            "{reason}: failed to close session after pane exit: {err}"
                        ));
                        self.set_message(
                            &format!("pane close failed: {err}"),
                            Duration::from_secs(2),
                        );
                    }
                }
            } else {
                self.write_log(&format!("{reason}: final pane closed, quitting"));
                self.should_quit = true;
            }
            return;
        }

        if self.current_session_mut().close_focused(cols, rows).is_ok() {
            self.apply_action_effects(ActionEffects {
                hook: Some(HookEvent::PaneClosed),
                ..ActionEffects::reorder()
            });
            self.write_log(&format!("{reason}: closed focused pane"));
            let message = self
                .undo_close_hint()
                .map(|hint| format!("pane closed ({hint} to restore)"))
                .unwrap_or_else(|| "pane closed".to_string());
            self.set_message(&message, Duration::from_secs(3));
        } else {
            self.set_message("pane close failed", Duration::from_secs(2));
        }
    }

    /// Close every pane whose process exited, wherever it lives — a split
    /// neighbor, a background window, or another session. The focused pane
    /// of the active view is handled by [`Self::close_focused_or_quit`]
    /// before this sweep runs so its quit/undo/status-message semantics
    /// stay unchanged; this catches the rest, the way ghostty closes a
    /// surface as soon as its child exits. A session left with only dead
    /// panes is killed outright (quitting when it was the last one).
    pub(super) fn close_exited_unfocused_panes(&mut self) {
        let (cols, rows) = self.current_effective_pane_dims();
        let mut session_index = 0;
        while session_index < self.sessions.len() {
            if self.should_quit {
                return;
            }
            let dead = self.sessions[session_index].session.closed_pane_ids();
            if dead.is_empty() {
                session_index += 1;
                continue;
            }

            let session_id = self.sessions[session_index].session_id.clone();
            if dead.len() >= self.sessions[session_index].session.pane_count() {
                match self.kill_session_by_index(session_index) {
                    // Ok(false) removed the session, shifting the next one
                    // into this slot; Ok(true) set should_quit for the loop
                    // guard. Either way the index stays put.
                    Ok(_) => {
                        self.write_log(&format!(
                            "pane process exited: closed dead session {session_id}"
                        ));
                    }
                    Err(err) => {
                        self.write_log(&format!(
                            "auto close of dead session {session_id} failed: {err}"
                        ));
                        session_index += 1;
                    }
                }
                continue;
            }

            // close_pane focuses its target before closing, which would
            // yank a live focused pane sitting next to the dead one.
            let focus_before = self.sessions[session_index].session.focused_pane_id();
            let mut closed_any = false;
            for pane_id in dead {
                let close_result = self.sessions[session_index]
                    .session
                    .close_pane(pane_id, cols, rows);
                match close_result {
                    Ok(()) => {
                        closed_any = true;
                        let session = &self.sessions[session_index];
                        let context = HookContext {
                            session_id: Some(session.session_id.clone()),
                            session_name: Some(session.session.session_name().to_string()),
                            pane_id: Some(pane_id),
                            ..HookContext::default()
                        };
                        self.emit_hook(HookEvent::PaneClosed, context);
                        self.write_log(&format!("pane process exited: closed pane {pane_id}"));
                    }
                    Err(err) => {
                        self.write_log(&format!("auto close of pane {pane_id} failed: {err}"));
                    }
                }
            }
            if closed_any {
                if let Some(pane_id) = focus_before
                    && self.sessions[session_index].session.pane_exists(pane_id)
                {
                    let _ = self.sessions[session_index].session.focus_pane_id(pane_id);
                }
                self.sync_tree_names();
                self.needs_render = true;
                self.needs_full_clear = true;
                self.persist_active_session_info();
            }
            session_index += 1;
        }
    }

    /// Key-chord hint for undoing the close just performed (e.g. `C-j u`),
    /// shown in the status message. `None` when nothing was retained or the
    /// restore action is unbound.
    fn undo_close_hint(&self) -> Option<String> {
        if !self.current_session().has_restorable_closed_pane() {
            return None;
        }
        let key = self
            .view
            .keys
            .prefix_key_for(&CommandAction::RestoreClosedPane)?;
        Some(format!("{} {key}", self.view.keys.prefix_key_display()))
    }

    /// Restore the most recently closed pane (undo close), if one is still
    /// within its retention window.
    pub(super) fn restore_last_closed_pane(&mut self) {
        let (cols, rows) = self.current_effective_pane_dims();
        match self
            .current_session_mut()
            .restore_last_closed_pane(cols, rows)
        {
            Ok(pane_id) => {
                self.apply_action_effects(ActionEffects {
                    hook: Some(HookEvent::PaneRestored),
                    ..ActionEffects::reorder()
                });
                self.write_log(&format!("restored closed pane {pane_id}"));
                self.set_message("pane restored", Duration::from_secs(2));
            }
            Err(err) => self.set_message(&err, Duration::from_secs(2)),
        }
    }

    pub(super) fn apply_action_effects(&mut self, effects: ActionEffects) {
        if effects.record_focus {
            self.record_focus_for_active_session();
        }
        if effects.sync_focus_history {
            self.sync_focus_history_for_active_session();
        }
        if effects.sync_tree_names {
            self.sync_tree_names();
        }
        if effects.full_clear {
            self.needs_full_clear = true;
        }
        if effects.persist_session_info {
            self.persist_active_session_info();
        }
        if effects.persist_runtime_state {
            self.persist_runtime_state();
        }
        if let Some(hook) = effects.hook {
            self.emit_hook(hook, self.current_hook_context());
        }
    }

    /// Move focus to the next (`delta > 0`) or previous (`delta < 0`) window in
    /// a flat list spanning every session, wrapping around the ends. This is the
    /// vertical-edge fallback for `Focus(Up/Down)`: at the last window of a
    /// session it steps into the first window of the next session, and past the
    /// final window it wraps to the very first one.
    pub(super) fn focus_global_window_relative(&mut self, delta: isize) {
        let order = self
            .sessions
            .iter()
            .enumerate()
            .flat_map(|(session_index, managed)| {
                managed
                    .session
                    .window_entries()
                    .into_iter()
                    .map(move |entry| (session_index, entry.index))
            })
            .collect::<Vec<_>>();
        if order.len() <= 1 {
            return;
        }

        let focused_window = self.current_session().focused_window_number();
        let current = order
            .iter()
            .position(|&(session_index, window_number)| {
                session_index == self.view.active_session && Some(window_number) == focused_window
            })
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(order.len() as isize) as usize;
        let (target_session, target_window) = order[next];

        if target_session != self.view.active_session {
            self.view.active_session = target_session;
        }
        if self
            .current_session_mut()
            .focus_window_number(target_window)
            .is_ok()
        {
            self.apply_action_effects(ActionEffects {
                full_clear: true,
                ..ActionEffects::focus()
            });
        }
    }

    pub(super) fn handle_action(&mut self, action: CommandAction) -> AppSignal {
        let (cols, rows) = self.current_effective_pane_dims();

        // Any action other than a repeated quit cancels a pending
        // quit confirmation.
        if !matches!(action, CommandAction::Quit) {
            self.cancel_quit_confirm();
        }

        match action {
            CommandAction::Split(axis) => {
                if self
                    .current_session_mut()
                    .split_focused(axis, cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::structure(HookEvent::PaneSplit));
                }
            }
            CommandAction::Focus(direction) => {
                if self
                    .current_session_mut()
                    .focus(direction, cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::focus());
                } else {
                    match direction {
                        Direction::Left => {
                            return self.handle_action(CommandAction::PrevSession);
                        }
                        Direction::Right => {
                            return self.handle_action(CommandAction::NextSession);
                        }
                        Direction::Up => self.focus_global_window_relative(-1),
                        Direction::Down => self.focus_global_window_relative(1),
                    }
                }
            }
            CommandAction::FocusNextPane => {
                if self.focus_next_pane_history() {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::FocusPrevPane => {
                if self.focus_prev_pane_history() {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::ClosePane => self.close_focused_or_quit("close pane"),
            CommandAction::RestoreClosedPane => self.restore_last_closed_pane(),
            CommandAction::Quit => self.request_quit(),
            CommandAction::DetachClient => return AppSignal::DetachClient,

            CommandAction::SystemTree => self.open_system_tree(),
            CommandAction::SideWindowTree => self.toggle_side_window_tree(),
            CommandAction::PeekAllWindows => self.open_peek_all_windows(),
            CommandAction::EnterCursorMode => self.open_cursor_mode(),
            CommandAction::SearchScrollback => self.open_cursor_mode_search(),
            CommandAction::LeaveCursorMode => {
                if matches!(self.view.input_mode, InputMode::CursorMode { .. }) {
                    self.view.input_mode = InputMode::Normal;
                } else {
                    self.set_message("cursor mode is not active", Duration::from_secs(2));
                }
            }
            CommandAction::CommandPalette => self.open_command_palette(),
            CommandAction::ShowKeybindings => self.open_keybindings(),
            CommandAction::NextWindow => {
                if self.current_session_mut().focus_next_window().is_ok() {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::PrevWindow => {
                if self.current_session_mut().focus_prev_window().is_ok() {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::SelectWindow(number) => {
                if self
                    .current_session_mut()
                    .focus_window_number(number)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::focus());
                }
            }
            CommandAction::NewWindow => {
                if self.current_session_mut().new_window(cols, rows).is_ok() {
                    self.apply_action_effects(ActionEffects::structure(HookEvent::WindowCreated));
                }
            }

            CommandAction::Resize(direction) => {
                if self
                    .current_session_mut()
                    .resize_focused(direction, 5, cols, rows)
                    .is_ok()
                {
                    self.needs_full_clear = true;
                }
            }
            CommandAction::EnterResizeMode => {
                self.view.input_mode = InputMode::ResizeMode;
            }
            CommandAction::SwapPane(direction) => {
                match self
                    .current_session_mut()
                    .swap_pane_in_direction(direction, cols, rows)
                {
                    Ok(()) => self.apply_action_effects(ActionEffects::reorder()),
                    Err(err) => self.set_message(&err, Duration::from_secs(2)),
                }
            }
            CommandAction::BreakPane => {
                match self
                    .current_session_mut()
                    .break_focused_pane_to_new_window(cols, rows)
                {
                    Ok(pane_id) => {
                        self.apply_action_effects(ActionEffects::structure(
                            HookEvent::WindowCreated,
                        ));
                        self.write_log(&format!("broke pane {pane_id} into new window"));
                        self.set_message("pane moved to new window", Duration::from_secs(2));
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("break pane failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            CommandAction::MovePaneToWindow(number) => {
                match self
                    .current_session_mut()
                    .move_focused_pane_to_window(number, cols, rows)
                {
                    Ok(pane_id) => {
                        self.apply_action_effects(ActionEffects::reorder());
                        self.write_log(&format!("moved pane {pane_id} to window {number}"));
                        self.set_message(
                            &format!("pane moved to window {number}"),
                            Duration::from_secs(2),
                        );
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("move pane failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            CommandAction::SwapPrevWindow => {
                if self.current_session_mut().swap_prev_window().is_ok() {
                    self.apply_action_effects(ActionEffects::reorder());
                }
            }
            CommandAction::SwapNextWindow => {
                if self.current_session_mut().swap_next_window().is_ok() {
                    self.apply_action_effects(ActionEffects::reorder());
                }
            }

            CommandAction::CopySelection => self.copy_active_text_selection(),
            CommandAction::CopyVersion => self.copy_spectra_version(),
            CommandAction::PasteImage => self.request_image_paste(),

            CommandAction::SaveLayout => self.save_active_layout(),
            CommandAction::WriteLog => self.write_log("manual log event"),
            CommandAction::WriteScrollback => self.write_active_scrollback(),
            CommandAction::OpenPaneBufferInEditor => self.open_current_pane_buffer_in_editor(),

            CommandAction::RenameSession => {
                let target = RenameTarget::Session {
                    session_index: self.view.active_session,
                };
                let buffer = self.rename_buffer_for_target(target);
                self.view.input_mode = InputMode::RenameTreeItem {
                    target,
                    buffer,
                    return_tree: None,
                };
            }
            CommandAction::NextSession => {
                if self.sessions.len() > 1 {
                    self.view.active_session = (self.view.active_session + 1) % self.sessions.len();
                    self.restore_focus_for_active_session_from_history();
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::PrevSession => {
                if self.sessions.len() > 1 {
                    if self.view.active_session == 0 {
                        self.view.active_session = self.sessions.len().saturating_sub(1);
                    } else {
                        self.view.active_session -= 1;
                    }
                    self.restore_focus_for_active_session_from_history();
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::NewSession => {
                self.create_session();
            }
            CommandAction::ToggleZoom => {
                if self
                    .current_session_mut()
                    .toggle_zoom_active_window(cols, rows)
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects::layout());
                }
            }
            CommandAction::ToggleSynchronizePanes => {
                if self
                    .current_session_mut()
                    .toggle_synchronize_panes_active_window()
                    .is_ok()
                {
                    self.apply_action_effects(ActionEffects {
                        persist_runtime_state: true,
                        ..Default::default()
                    });
                }
            }
            CommandAction::ReloadConfig => match self.reload_config_from_path(None) {
                Ok(message) => self.set_message(&message, Duration::from_secs(3)),
                Err(err) => self.set_message(&err, Duration::from_secs(3)),
            },
            CommandAction::CreateDefaultConfig => {
                let path = config::config_path();
                match self.create_default_config_at_path(&path) {
                    Ok(message) => self.set_message(&message, Duration::from_secs(3)),
                    Err(err) => self.set_message(&err, Duration::from_secs(3)),
                }
            }
            CommandAction::OpenConfigInEditor => self.open_config_in_editor(),
            CommandAction::EnterLockMode => {
                self.view.locked_input = true;
                self.set_message("lock mode on", Duration::from_secs(2));
            }
            CommandAction::LeaveLockMode => {
                self.view.locked_input = false;
                self.set_message("lock mode off", Duration::from_secs(2));
            }
            CommandAction::KillSession => {
                match self.kill_session_by_index(self.view.active_session) {
                    Ok(shutdown) => {
                        if shutdown {
                            self.set_message(
                                "killed final session; shutting down",
                                Duration::from_secs(2),
                            );
                        } else {
                            self.sync_tree_names();
                            self.needs_full_clear = true;
                            self.set_message("session killed", Duration::from_secs(2));
                        }
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("kill session failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            CommandAction::RunShell(command) => self.run_shell_binding(&command),
            CommandAction::CloseWindow => {
                match self.current_session_mut().close_active_window(cols, rows) {
                    Ok(()) => {
                        self.apply_action_effects(ActionEffects::reorder());
                        self.set_message("window closed", Duration::from_secs(2));
                    }
                    Err(err) => {
                        self.set_message(
                            &format!("close window failed: {err}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
        }
        AppSignal::None
    }

    pub(super) fn create_session(&mut self) {
        match self.create_session_internal() {
            Ok(_) => {
                self.set_message("session created", Duration::from_secs(2));
            }
            Err(err) => {
                self.set_message(
                    &format!("create session failed: {err}"),
                    Duration::from_secs(3),
                );
            }
        }
    }

    pub(super) fn create_session_internal(&mut self) -> Result<String, String> {
        let ordinal = self.next_session_ordinal;
        self.next_session_ordinal += 1;

        let mut options = self.session_template.clone();
        options.session_name = format!("{}-{ordinal}", self.session_template.session_name);
        options.session_id = session_id_for(&options.session_name, ordinal);

        let (cols, rows) = self.current_effective_pane_dims();
        let mut session = SessionManager::new(options, cols, rows)
            .map_err(|err| format!("create session failed: {err}"))?;
        session
            .resize(cols, rows)
            .map_err(|err| format!("resize session failed: {err}"))?;

        let session_id = session_id_for(session.session_name(), ordinal);
        self.sessions.push(ManagedSession {
            ordinal,
            session_id: session_id.clone(),
            session,
            window_names: HashMap::new(),
            pane_names: HashMap::new(),
            window_auto_names: HashMap::new(),
            pane_auto_names: HashMap::new(),
            terminal_titles: HashMap::new(),
            cwd_fallbacks: HashMap::new(),
            agents: AgentTracking::default(),
        });
        self.view.active_session = self.sessions.len().saturating_sub(1);
        self.record_focus_for_active_session();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::SessionCreated, self.current_hook_context());
        self.write_log("created session");
        Ok(session_id)
    }

    pub(super) fn reload_config_from_path(&mut self, path: Option<&str>) -> Result<String, String> {
        let path = path.map(PathBuf::from).unwrap_or_else(config::config_path);
        let loaded = config::load_from_path(Path::new(&path))
            .map_err(|err| format!("source-file failed: {err}"))?;
        self.apply_loaded_config(loaded);
        self.persist_runtime_state();
        self.reload_plugins();
        self.emit_hook(HookEvent::ConfigReloaded, self.current_hook_context());
        let message = format!("config reloaded: {}", path.display());
        self.write_log(&message);
        Ok(message)
    }

    pub(super) fn create_default_config_at_path(&mut self, path: &Path) -> Result<String, String> {
        let merged = if path.exists() {
            let contents = std::fs::read_to_string(path)
                .map_err(|err| format!("config read failed ({}): {err}", path.display()))?;
            toml::from_str::<config::AppConfig>(&contents)
                .map_err(|err| format!("config parse failed: {err}"))?
        } else {
            config::AppConfig::default()
        };

        let contents = toml::to_string_pretty(&merged)
            .map_err(|err| format!("config serialize failed: {err}"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "config directory create failed ({}): {err}",
                    parent.display()
                )
            })?;
        }

        std::fs::write(path, contents)
            .map_err(|err| format!("config write failed ({}): {err}", path.display()))?;

        self.apply_loaded_config(merged);
        self.persist_runtime_state();
        self.reload_plugins();
        self.emit_hook(HookEvent::ConfigReloaded, self.current_hook_context());
        let message = format!("config written: {}", path.display());
        self.write_log(&message);
        Ok(message)
    }

    fn apply_loaded_config(&mut self, loaded: config::AppConfig) {
        let keys = KeyMapper::with_config(
            loaded.prefix.as_deref(),
            loaded.prefix_sticky,
            &loaded.prefix_bindings,
            &loaded.global_bindings,
        );
        self.key_template = keys.clone();
        self.view.keys = keys.clone();
        for state in self.inactive_client_states.values_mut() {
            state.keys = keys.clone();
        }
        self.status_format = loaded
            .status
            .format
            .clone()
            .unwrap_or_else(|| DEFAULT_STATUS_FORMAT.to_string());
        self.status_style = status_style_from_config(&loaded.status);
        self.sidebar_formats = SidebarFormats::from_config(&loaded.sidebar);
        self.hooks = loaded.hooks.clone();
        self.editor_command = normalize_editor_command(loaded.editor.clone());
        self.agent_notify = loaded.agent.notify;
        self.command_finish = loaded.command_finish;
        self.ime = loaded.ime.clone();

        let suppress_prompt_eol_marker = loaded.shell.suppress_prompt_eol_marker;
        self.session_template.suppress_prompt_eol_marker = suppress_prompt_eol_marker;
        let allow_passthrough = loaded.terminal.allow_passthrough;
        self.session_template.allow_passthrough = allow_passthrough;
        let scrollback_lines = loaded.terminal.scrollback_lines;
        self.session_template.scrollback_lines = scrollback_lines;
        let handoff_replay_bytes = loaded.pane.handoff_replay_bytes;
        self.session_template.handoff_replay_bytes = handoff_replay_bytes;
        let undo_close_timeout = Duration::from_secs(loaded.pane.undo_close_seconds);
        self.session_template.undo_close_timeout = undo_close_timeout;
        let new_cwd = loaded.shell.new_cwd.clone();
        self.session_template.new_cwd = new_cwd.clone();
        for managed in &mut self.sessions {
            managed
                .session
                .set_suppress_prompt_eol_marker(suppress_prompt_eol_marker);
            managed.session.set_allow_passthrough(allow_passthrough);
            managed.session.set_scrollback_lines(scrollback_lines);
            managed
                .session
                .set_handoff_replay_bytes(handoff_replay_bytes);
            managed.session.set_undo_close_timeout(undo_close_timeout);
            managed.session.set_new_cwd_policy(new_cwd.clone());
        }

        self.mouse_enabled = loaded.mouse.enabled;
        self.open_click = loaded.mouse.open_click;
        self.open_click_commands =
            super::open_click::normalize_open_click_commands(&loaded.mouse.open_click_commands);
        if !self.mouse_enabled {
            self.view.mouse_drag = None;
        }
    }
}
