use super::*;

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
            self.sync_tree_names();
            self.needs_full_clear = true;
            self.persist_active_session_info();
            self.emit_hook(HookEvent::PaneClosed, self.current_hook_context());
            self.write_log(&format!("{reason}: closed focused pane"));
            self.set_message("pane closed", Duration::from_secs(2));
        } else {
            self.set_message("pane close failed", Duration::from_secs(2));
        }
    }

    fn apply_action_effects(&mut self, effects: ActionEffects) {
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

    pub(super) fn handle_action(&mut self, action: CommandAction) -> AppSignal {
        let (cols, rows) = self.current_effective_pane_dims();

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
                        Direction::Up => {
                            if self.current_session_mut().focus_prev_window().is_ok() {
                                self.apply_action_effects(ActionEffects::focus());
                            }
                        }
                        Direction::Down => {
                            if self.current_session_mut().focus_next_window().is_ok() {
                                self.apply_action_effects(ActionEffects::focus());
                            }
                        }
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
            CommandAction::ClosePane => {
                if self.current_session_mut().close_focused(cols, rows).is_ok() {
                    self.apply_action_effects(ActionEffects {
                        hook: Some(HookEvent::PaneClosed),
                        ..ActionEffects::reorder()
                    });
                }
            }
            CommandAction::Quit => self.should_quit = true,
            CommandAction::DetachClient => return AppSignal::DetachClient,

            CommandAction::SystemTree => self.open_system_tree(),
            CommandAction::SideWindowTree => self.toggle_side_window_tree(),
            CommandAction::PeekAllWindows => self.open_peek_all_windows(),
            CommandAction::EnterCursorMode => self.open_cursor_mode(),
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
        self.hooks = loaded.hooks.clone();
        self.editor_command = normalize_editor_command(loaded.editor.clone());
        self.agent_notify = loaded.agent.notify;

        let suppress_prompt_eol_marker = loaded.shell.suppress_prompt_eol_marker;
        self.session_template.suppress_prompt_eol_marker = suppress_prompt_eol_marker;
        let allow_passthrough = loaded.terminal.allow_passthrough;
        self.session_template.allow_passthrough = allow_passthrough;
        for managed in &mut self.sessions {
            managed
                .session
                .set_suppress_prompt_eol_marker(suppress_prompt_eol_marker);
            managed.session.set_allow_passthrough(allow_passthrough);
        }

        self.mouse_enabled = loaded.mouse.enabled;
        if !self.mouse_enabled {
            self.view.mouse_drag = None;
        }
    }
}
