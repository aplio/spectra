use super::*;

impl App {
    pub(super) fn sync_focus_history_for_active_session(&mut self) {
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let focused = self.current_session().focused_pane_id();

        let should_remove = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id.clone())
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            if let Some(pane_id) = focused {
                history.sync_index_from_current(pane_id);
            }
            history.is_empty()
        };

        if should_remove {
            self.view.pane_histories_by_session.remove(&session_id);
        }
    }

    pub(super) fn record_focus_for_active_session(&mut self) {
        let Some(focused) = self.current_session().focused_pane_id() else {
            return;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let history = self
            .view
            .pane_histories_by_session
            .entry(session_id)
            .or_default();
        history.prune_invalid(&valid_pane_ids);
        history.record_focus(focused);
    }

    pub(super) fn restore_focus_for_active_session_from_history(&mut self) {
        if self.sessions.is_empty() {
            return;
        }

        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let focused = self.current_session().focused_pane_id();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            if history.is_empty() {
                if let Some(pane_id) = focused {
                    history.record_focus(pane_id);
                }
                None
            } else {
                history
                    .current_pane()
                    .or(focused)
                    .or_else(|| history.pane_ids.last().copied())
            }
        };

        if let Some(target_pane_id) = target {
            let _ = self.current_session_mut().focus_pane_id(target_pane_id);
        }
        self.sync_focus_history_for_active_session();
    }

    pub(super) fn focus_prev_pane_history(&mut self) -> bool {
        let Some(current_pane_id) = self.current_session().focused_pane_id() else {
            return false;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            history.sync_index_from_current(current_pane_id);
            history.prev_from(current_pane_id)
        };
        let Some(target_pane_id) = target else {
            return false;
        };

        if self
            .current_session_mut()
            .focus_pane_id(target_pane_id)
            .is_err()
        {
            return false;
        }
        self.sync_focus_history_for_active_session();
        self.current_session().focused_pane_id() != Some(current_pane_id)
    }

    pub(super) fn focus_next_pane_history(&mut self) -> bool {
        let Some(current_pane_id) = self.current_session().focused_pane_id() else {
            return false;
        };
        let session_id = self.current_session_id().to_string();
        let valid_pane_ids = self
            .current_session()
            .all_pane_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        let target = {
            let history = self
                .view
                .pane_histories_by_session
                .entry(session_id)
                .or_default();
            history.prune_invalid(&valid_pane_ids);
            history.sync_index_from_current(current_pane_id);
            history.next_from(current_pane_id)
        };
        let Some(target_pane_id) = target else {
            return false;
        };

        if self
            .current_session_mut()
            .focus_pane_id(target_pane_id)
            .is_err()
        {
            return false;
        }
        self.sync_focus_history_for_active_session();
        self.current_session().focused_pane_id() != Some(current_pane_id)
    }

    pub(super) fn restore_active_client_focus_profile(&mut self, identity: &str) {
        let Some(profile) = self.client_focus_profiles.get(identity).cloned() else {
            return;
        };
        self.apply_persisted_client_focus_state(profile);
    }

    fn restore_client_focus_profile_for_client(&mut self, client_id: ClientId, identity: &str) {
        let Some(profile) = self.client_focus_profiles.get(identity).cloned() else {
            return;
        };
        self.with_client_context(client_id, move |app| {
            app.apply_persisted_client_focus_state(profile);
        });
    }

    fn apply_persisted_client_focus_state(&mut self, profile: PersistedClientFocusState) {
        let mut pane_histories_by_session =
            default_pane_histories_for_managed_sessions(&self.sessions);
        for (session_id, history) in profile.pane_histories_by_session {
            pane_histories_by_session.insert(session_id, PaneFocusHistory::from_snapshot(history));
        }
        prune_pane_histories_for_managed_sessions(&mut pane_histories_by_session, &self.sessions);
        self.view.pane_histories_by_session = pane_histories_by_session;

        if let Some(session_id) = profile.active_session_id
            && let Some(session_index) = self.session_index_for_id(&session_id)
        {
            self.view.active_session = session_index;
        }

        self.restore_focus_for_active_session_from_history();
    }

    fn capture_client_focus_profile(&mut self, client_id: ClientId) {
        if client_id == self.active_client_id {
            self.capture_active_client_focus_profile();
            return;
        }

        let Some(identity) = self.client_identities.get(&client_id).cloned() else {
            return;
        };
        let Some(state) = self.inactive_client_states.get(&client_id) else {
            return;
        };
        let profile = persisted_client_focus_state_from_state(
            state.active_session,
            &state.pane_histories_by_session,
            &self.sessions,
        );
        self.client_focus_profiles.insert(identity, profile);
    }

    pub(super) fn capture_active_client_focus_profile(&mut self) {
        self.sync_focus_history_for_active_session();
        let Some(identity) = self.client_identities.get(&self.active_client_id).cloned() else {
            return;
        };
        let profile = persisted_client_focus_state_from_state(
            self.view.active_session,
            &self.view.pane_histories_by_session,
            &self.sessions,
        );
        self.client_focus_profiles.insert(identity, profile);
    }

    pub(super) fn collect_client_focus_profiles(
        &self,
    ) -> HashMap<String, PersistedClientFocusState> {
        let mut profiles = self.client_focus_profiles.clone();

        if let Some(identity) = self.client_identities.get(&self.active_client_id) {
            let profile = persisted_client_focus_state_from_state(
                self.view.active_session,
                &self.view.pane_histories_by_session,
                &self.sessions,
            );
            profiles.insert(identity.clone(), profile);
        }

        for (client_id, state) in &self.inactive_client_states {
            let Some(identity) = self.client_identities.get(client_id) else {
                continue;
            };
            let profile = persisted_client_focus_state_from_state(
                state.active_session,
                &state.pane_histories_by_session,
                &self.sessions,
            );
            profiles.insert(identity.clone(), profile);
        }

        profiles.retain(|_, profile| {
            profile.active_session_id.is_some() || !profile.pane_histories_by_session.is_empty()
        });
        profiles
    }

    fn default_client_view_state(&self, cols: u16, rows: u16) -> ClientViewState {
        ClientViewState {
            keys: self.key_template.clone(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            selection_autoscroll: None,
            click_chain: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            pending_image_paste_request: false,
            cols,
            rows,
            active_session: self.view.active_session,
            pane_histories_by_session: self.view.pane_histories_by_session.clone(),
            side_window_tree_open: self.view.side_window_tree_open,
            search_history: Vec::new(),
        }
    }

    fn take_active_client_state(&mut self) -> ClientViewState {
        self.sync_focus_history_for_active_session();
        let reset = self.default_client_view_state_reset();
        std::mem::replace(&mut self.view, reset)
    }

    fn default_client_view_state_reset(&self) -> ClientViewState {
        ClientViewState {
            keys: self.key_template.clone(),
            input_mode: InputMode::Normal,
            status_message: None,
            locked_input: false,
            mouse_drag: None,
            text_selection: None,
            selection_autoscroll: None,
            click_chain: None,
            pending_clipboard_ansi: Vec::new(),
            pending_passthrough_ansi: Vec::new(),
            pending_image_paste_request: false,
            cols: self.view.cols,
            rows: self.view.rows,
            active_session: 0,
            pane_histories_by_session: HashMap::new(),
            side_window_tree_open: false,
            search_history: Vec::new(),
        }
    }

    fn install_active_client_state(&mut self, mut state: ClientViewState) {
        prune_pane_histories_for_managed_sessions(
            &mut state.pane_histories_by_session,
            &self.sessions,
        );
        let max_session_index = self.sessions.len().saturating_sub(1);
        state.active_session = state.active_session.min(max_session_index);
        self.view = state;
        self.prune_side_window_tree_state();
        self.restore_focus_for_active_session_from_history();
    }

    fn switch_active_client(&mut self, client_id: ClientId) {
        if self.active_client_id == client_id {
            return;
        }

        let previous_id = self.active_client_id;
        self.capture_active_client_focus_profile();
        let previous_state = self.take_active_client_state();
        self.inactive_client_states
            .insert(previous_id, previous_state);

        let next_state = self
            .inactive_client_states
            .remove(&client_id)
            .unwrap_or_else(|| self.default_client_view_state(self.view.cols, self.view.rows));
        self.install_active_client_state(next_state);
        self.active_client_id = client_id;
        self.capture_active_client_focus_profile();
    }

    pub(super) fn with_client_context<T>(
        &mut self,
        client_id: ClientId,
        action: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous_client_id = self.active_client_id;
        self.switch_active_client(client_id);
        let result = action(self);
        self.switch_active_client(previous_client_id);
        result
    }

    pub fn register_client(&mut self, client_id: ClientId, cols: u16, rows: u16) {
        if self.active_client_id == client_id {
            self.view.cols = cols;
            self.view.rows = rows;
            return;
        }

        if let Some(state) = self.inactive_client_states.get_mut(&client_id) {
            state.cols = cols;
            state.rows = rows;
            return;
        }

        self.inactive_client_states
            .insert(client_id, self.default_client_view_state(cols, rows));
    }

    /// Cache the default fg/bg colors reported by the most recently
    /// attached client's host terminal and push them to every pane so
    /// guest OSC 10/11 queries can be answered. The template used for
    /// future sessions/panes is updated too. Last writer wins; a client
    /// that reported nothing resets the cache back to "unknown".
    pub fn set_host_colors(&mut self, colors: HostColors) {
        self.session_template.host_colors = colors;
        for managed in &mut self.sessions {
            managed.session.set_host_colors(colors);
        }
    }

    /// Host terminal default colors currently cached for OSC 10/11.
    pub fn host_colors(&self) -> HostColors {
        self.session_template.host_colors
    }

    pub fn register_client_identity(&mut self, client_id: ClientId, identity: Option<String>) {
        let identity =
            normalize_client_identity(identity).unwrap_or_else(|| format!("client-{client_id}"));
        self.client_identities.insert(client_id, identity.clone());
        self.restore_client_focus_profile_for_client(client_id, &identity);
        self.capture_client_focus_profile(client_id);
    }

    pub fn unregister_client(&mut self, client_id: ClientId) {
        if self.active_client_id == client_id {
            if client_id == LOCAL_CLIENT_ID {
                return;
            }

            self.capture_active_client_focus_profile();
            let _ = self.take_active_client_state();
            let fallback = self
                .inactive_client_states
                .remove(&LOCAL_CLIENT_ID)
                .unwrap_or_else(|| self.default_client_view_state(self.view.cols, self.view.rows));
            self.install_active_client_state(fallback);
            self.active_client_id = LOCAL_CLIENT_ID;
            self.client_identities.remove(&client_id);
            return;
        }

        self.capture_client_focus_profile(client_id);
        self.inactive_client_states.remove(&client_id);
        self.client_identities.remove(&client_id);
    }

    pub fn handle_key_event_for_client(
        &mut self,
        client_id: ClientId,
        key: KeyEvent,
    ) -> io::Result<AppSignal> {
        self.with_client_context(client_id, move |app| app.handle_key(key))
    }

    pub fn handle_action_for_client(
        &mut self,
        client_id: ClientId,
        action: CommandAction,
    ) -> AppSignal {
        self.with_client_context(client_id, move |app| {
            app.needs_render = true;
            app.handle_action(action)
        })
    }

    pub fn handle_paste_text_for_client(
        &mut self,
        client_id: ClientId,
        text: String,
    ) -> io::Result<AppSignal> {
        self.with_client_context(client_id, move |app| app.handle_paste(text))
    }

    pub fn handle_mouse_event_for_client(
        &mut self,
        client_id: ClientId,
        mouse: MouseEvent,
    ) -> io::Result<()> {
        self.with_client_context(client_id, move |app| {
            app.handle_mouse(mouse);
            Ok(())
        })
    }

    /// Handle a `ClientMessage::PasteImage` reply: `image` is
    /// `Some((format, bytes))` when the client's clipboard held an image.
    pub fn handle_paste_image_for_client(
        &mut self,
        client_id: ClientId,
        image: Option<(String, Vec<u8>)>,
    ) -> io::Result<AppSignal> {
        self.with_client_context(client_id, move |app| app.paste_clipboard_image(image))
    }

    /// Take (and clear) the client's pending paste-image request flag; the
    /// server loop sends one `PasteImageRequest` per taken flag.
    pub fn take_pending_image_paste_request_for_client(&mut self, client_id: ClientId) -> bool {
        if self.active_client_id == client_id {
            return std::mem::take(&mut self.view.pending_image_paste_request);
        }
        self.inactive_client_states
            .get_mut(&client_id)
            .map(|state| std::mem::take(&mut state.pending_image_paste_request))
            .unwrap_or(false)
    }

    pub fn take_pending_clipboard_ansi_for_client(&mut self, client_id: ClientId) -> Vec<String> {
        if self.active_client_id == client_id {
            return std::mem::take(&mut self.view.pending_clipboard_ansi);
        }
        self.inactive_client_states
            .get_mut(&client_id)
            .map(|state| std::mem::take(&mut state.pending_clipboard_ansi))
            .unwrap_or_default()
    }

    pub fn take_pending_passthrough_ansi_for_client(&mut self, client_id: ClientId) -> Vec<String> {
        if self.active_client_id == client_id {
            return std::mem::take(&mut self.view.pending_passthrough_ansi);
        }
        self.inactive_client_states
            .get_mut(&client_id)
            .map(|state| std::mem::take(&mut state.pending_passthrough_ansi))
            .unwrap_or_default()
    }

    pub fn handle_client_resize_event(
        &mut self,
        client_id: ClientId,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        self.with_client_context(client_id, |app| {
            app.view.cols = cols;
            app.view.rows = rows;
            app.resize_sessions_to_max_client_viewport()?;
            app.needs_render = true;
            Ok(())
        })
    }

    pub fn apply_attach_target_for_client(
        &mut self,
        client_id: ClientId,
        target: &AttachTarget,
    ) -> Result<(), String> {
        self.with_client_context(client_id, |app| app.apply_attach_target(target))
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> io::Result<AppSignal> {
        self.handle_key_event_for_client(LOCAL_CLIENT_ID, key)
    }

    pub fn handle_paste_text(&mut self, text: String) -> io::Result<AppSignal> {
        self.handle_paste_text_for_client(LOCAL_CLIENT_ID, text)
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) -> io::Result<()> {
        self.handle_mouse_event_for_client(LOCAL_CLIENT_ID, mouse)
    }

    pub fn handle_resize_event(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.with_client_context(LOCAL_CLIENT_ID, |app| app.handle_resize(cols, rows))
    }
}
