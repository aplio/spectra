use super::*;

impl App {
    /// Read-only session snapshot for the JSON-RPC API (`session.list`).
    pub fn api_sessions(&self) -> Vec<crate::api::SessionInfo> {
        self.sessions
            .iter()
            .enumerate()
            .map(|(index, managed)| crate::api::SessionInfo {
                session_id: managed.session_id.clone(),
                name: managed.session.session_name().to_string(),
                ordinal: managed.ordinal,
                active: index == self.view.active_session,
                windows: managed.session.window_count(),
            })
            .collect()
    }

    /// Read-only pane snapshot for the JSON-RPC API (`pane.list`).
    pub fn api_panes(&self, session_id: Option<&str>) -> Vec<crate::api::PaneInfo> {
        let mut panes = Vec::new();
        for managed in &self.sessions {
            if session_id.is_some_and(|filter| filter != managed.session_id) {
                continue;
            }
            let focused_pane = managed.session.focused_pane_id();
            for entry in managed.session.window_entries() {
                for pane_id in &entry.pane_ids {
                    panes.push(crate::api::PaneInfo {
                        pane_id: *pane_id,
                        session_id: managed.session_id.clone(),
                        window: entry.index,
                        focused: entry.focused && focused_pane == Some(*pane_id),
                        title: Self::api_pane_title(managed, *pane_id),
                        agent: managed.agents.statuses.get(pane_id).map(|status| {
                            let state = status
                                .display_state(managed.agents.seen.contains(pane_id))
                                .as_str()
                                .to_string();
                            crate::api::AgentInfo {
                                kind: status.kind.clone(),
                                state,
                            }
                        }),
                    });
                }
            }
        }
        panes
    }

    fn api_pane_title(managed: &ManagedSession, pane_id: usize) -> Option<String> {
        managed
            .pane_names
            .get(&pane_id)
            .or_else(|| managed.pane_auto_names.get(&pane_id))
            .cloned()
            .or_else(|| Self::resolve_auto_pane_name(managed, pane_id))
    }

    /// Read-only pane text for the JSON-RPC API (`pane.read`).
    ///
    /// Without `lines`, returns the pane's visible screen text; with
    /// `lines: N`, returns the last N lines including scrollback.
    pub fn api_pane_read(
        &self,
        pane_id: usize,
        session_id: Option<&str>,
        lines: Option<usize>,
    ) -> Option<String> {
        for managed in &self.sessions {
            if session_id.is_some_and(|filter| filter != managed.session_id) {
                continue;
            }
            if !managed.session.pane_exists(pane_id) {
                continue;
            }
            let max_lines = lines.or_else(|| managed.session.pane_screen_rows(pane_id))?;
            return managed
                .session
                .pane_history_tail_lines(pane_id, max_lines)
                .map(|tail| tail.join("\n"));
        }
        None
    }

    /// `pane.send_keys` for the JSON-RPC API: write raw text bytes verbatim
    /// to one pane's PTY (no key encoding; same semantics as CLI send-keys).
    pub fn api_send_keys(
        &mut self,
        pane_id: usize,
        session_id: Option<&str>,
        text: &str,
    ) -> Result<(), String> {
        for managed in &mut self.sessions {
            if session_id.is_some_and(|filter| filter != managed.session_id) {
                continue;
            }
            if !managed.session.pane_exists(pane_id) {
                continue;
            }
            return managed
                .session
                .send_to_pane(pane_id, text.as_bytes())
                .map_err(|err| format!("send-keys failed: {err}"));
        }
        Err("pane not found".to_string())
    }

    /// `pane.split` for the JSON-RPC API: focus the target pane (default:
    /// the currently focused pane of the active session), split it, and
    /// return the new pane id. Reuses the same path as the CLI
    /// `split-window` command (sizing via current effective dims, same
    /// action effects and `pane_split` hook).
    /// Resolve an API request's target session/pane: select the session that
    /// owns `pane_id` (or the named/active session) and focus the pane.
    fn api_focus_target(
        &mut self,
        pane_id: Option<usize>,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let session_index = match pane_id {
            Some(pane_id) => self
                .sessions
                .iter()
                .position(|managed| {
                    session_id.is_none_or(|filter| filter == managed.session_id)
                        && managed.session.pane_exists(pane_id)
                })
                .ok_or_else(|| "pane not found".to_string())?,
            None => match session_id {
                Some(filter) => self
                    .sessions
                    .iter()
                    .position(|managed| managed.session_id == filter)
                    .ok_or_else(|| format!("session `{filter}` not found"))?,
                None => self.view.active_session,
            },
        };
        if session_index != self.view.active_session {
            self.select_session(session_index);
        }
        if let Some(pane_id) = pane_id {
            self.current_session_mut().focus_pane_id(pane_id)?;
        }
        Ok(())
    }

    /// Session index owning `pane_id` (optionally restricted to one session).
    fn api_session_index_for_pane(
        &self,
        pane_id: usize,
        session_id: Option<&str>,
    ) -> Result<usize, String> {
        self.sessions
            .iter()
            .position(|managed| {
                session_id.is_none_or(|filter| filter == managed.session_id)
                    && managed.session.pane_exists(pane_id)
            })
            .ok_or_else(|| "pane not found".to_string())
    }

    /// Session index by API session id, defaulting to the active session.
    fn api_session_index(&self, session_id: Option<&str>) -> Result<usize, String> {
        match session_id {
            Some(filter) => self
                .sessions
                .iter()
                .position(|managed| managed.session_id == filter)
                .ok_or_else(|| format!("session `{filter}` not found")),
            None => Ok(self.view.active_session),
        }
    }

    /// Shared post-mutation bookkeeping for API methods that restructure
    /// panes or windows.
    fn api_apply_structure_effects(&mut self) {
        self.record_focus_for_active_session();
        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
    }

    pub fn api_split_pane(
        &mut self,
        pane_id: Option<usize>,
        session_id: Option<&str>,
        axis: crate::ui::window_manager::SplitAxis,
    ) -> Result<usize, String> {
        self.api_focus_target(pane_id, session_id)
            .map_err(|err| format!("split-window failed: {err}"))?;

        let (cols, rows) = self.current_effective_pane_dims();
        self.current_session_mut()
            .split_focused(axis, cols, rows)
            .map_err(|err| format!("split-window failed: {err}"))?;
        self.record_focus_for_active_session();
        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::PaneSplit, self.current_hook_context());
        // The split focuses the new pane, so the focused pane id is the
        // freshly created one.
        self.current_session()
            .focused_pane_id()
            .ok_or_else(|| "split-window failed: no focused pane after split".to_string())
    }

    /// `pane.swap` for the JSON-RPC API: focus the target pane (default: the
    /// focused pane) and swap it with its nearest neighbor in `direction`,
    /// keeping the split shape and both PTYs.
    pub fn api_swap_pane(
        &mut self,
        pane_id: Option<usize>,
        session_id: Option<&str>,
        direction: crate::ui::window_manager::Direction,
    ) -> Result<(), String> {
        self.api_focus_target(pane_id, session_id)?;
        let (cols, rows) = self.current_effective_pane_dims();
        self.current_session_mut()
            .swap_pane_in_direction(direction, cols, rows)?;
        self.sync_focus_history_for_active_session();
        self.api_apply_structure_effects();
        Ok(())
    }

    /// `pane.move` (to window / new window) for the JSON-RPC API: relocate
    /// the target pane within its session, PTY intact. `to_window: None`
    /// breaks the pane out into a new window. Returns the pane id and the
    /// window number it now lives in.
    pub fn api_move_pane_in_session(
        &mut self,
        pane_id: Option<usize>,
        session_id: Option<&str>,
        to_window: Option<usize>,
    ) -> Result<(usize, usize), String> {
        self.api_focus_target(pane_id, session_id)?;
        let (cols, rows) = self.current_effective_pane_dims();
        let moved = match to_window {
            Some(number) => self
                .current_session_mut()
                .move_focused_pane_to_window(number, cols, rows)?,
            None => {
                let moved = self
                    .current_session_mut()
                    .break_focused_pane_to_new_window(cols, rows)?;
                self.emit_hook(HookEvent::WindowCreated, self.current_hook_context());
                moved
            }
        };
        self.sync_focus_history_for_active_session();
        self.api_apply_structure_effects();
        let window = self
            .current_session()
            .focused_window_number()
            .ok_or_else(|| "move pane failed: no focused window".to_string())?;
        Ok((moved, window))
    }

    /// `pane.move` (to session) for the JSON-RPC API: detach the pane from
    /// its session and adopt it into `target_session` as a new window, PTY
    /// intact. Pane ids are per-session, so the pane gets a new id in the
    /// target session; returns it.
    pub fn api_move_pane_to_session(
        &mut self,
        pane_id: usize,
        session_id: Option<&str>,
        target_session: &str,
    ) -> Result<usize, String> {
        let source_index = self.api_session_index_for_pane(pane_id, session_id)?;
        let target_index = self
            .sessions
            .iter()
            .position(|managed| managed.session_id == target_session)
            .ok_or_else(|| format!("session `{target_session}` not found"))?;
        if source_index == target_index {
            return Err("pane is already in that session".to_string());
        }

        let (cols, rows) = self.current_effective_pane_dims();
        let pane = self.sessions[source_index]
            .session
            .take_pane_for_transfer(pane_id, cols, rows)?;
        let new_pane_id = self.sessions[target_index]
            .session
            .adopt_pane_as_window(pane, cols, rows)?;

        // Per-pane bookkeeping follows the pane to its new id; detection
        // state is rebuilt from scratch in the target session.
        let source = &mut self.sessions[source_index];
        let name = source.pane_names.remove(&pane_id);
        source.pane_auto_names.remove(&pane_id);
        let title = source.terminal_titles.remove(&pane_id);
        let cwd_fallback = source.cwd_fallbacks.remove(&pane_id);
        source
            .agents
            .prune_closed_panes(|candidate| candidate != pane_id);
        let target = &mut self.sessions[target_index];
        if let Some(name) = name {
            target.pane_names.insert(new_pane_id, name);
        }
        if let Some(title) = title {
            target.terminal_titles.insert(new_pane_id, title);
        }
        if let Some(cwd_fallback) = cwd_fallback {
            target.cwd_fallbacks.insert(new_pane_id, cwd_fallback);
        }

        if target_index != self.view.active_session {
            self.select_session(target_index);
        }
        let _ = self.current_session_mut().focus_pane_id(new_pane_id);
        self.emit_hook(HookEvent::WindowCreated, self.current_hook_context());
        self.api_apply_structure_effects();
        Ok(new_pane_id)
    }

    /// `layout.export` for the JSON-RPC API: the split tree of one window
    /// (default: the session's focused window) as a portable layout.
    pub fn api_layout_export(
        &self,
        session_id: Option<&str>,
        window: Option<usize>,
    ) -> Result<(String, usize, crate::ui::window_manager::LayoutTree), String> {
        let index = self.api_session_index(session_id)?;
        let managed = &self.sessions[index];
        let number = match window {
            Some(number) => number,
            None => managed
                .session
                .focused_window_number()
                .ok_or_else(|| "session has no windows".to_string())?,
        };
        let tree = managed.session.export_window_layout(number)?;
        Ok((managed.session_id.clone(), number, tree))
    }

    /// `layout.apply` for the JSON-RPC API: rearrange one window (default:
    /// the session's focused window) into the given layout. The layout's
    /// leaves must reference exactly the panes currently in that window.
    pub fn api_layout_apply(
        &mut self,
        session_id: Option<&str>,
        window: Option<usize>,
        tree: &crate::ui::window_manager::LayoutTree,
    ) -> Result<(), String> {
        let index = self.api_session_index(session_id)?;
        let number = match window {
            Some(number) => number,
            None => self.sessions[index]
                .session
                .focused_window_number()
                .ok_or_else(|| "session has no windows".to_string())?,
        };
        let (cols, rows) = self.current_effective_pane_dims();
        self.sessions[index]
            .session
            .apply_window_layout(number, tree, cols, rows)?;
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        Ok(())
    }

    /// `layout.set_split_ratio` for the JSON-RPC API: set the first-child
    /// share (percent) of the split directly containing `pane_id`.
    pub fn api_layout_set_split_ratio(
        &mut self,
        pane_id: usize,
        session_id: Option<&str>,
        ratio_percent: u8,
    ) -> Result<(), String> {
        let index = self.api_session_index_for_pane(pane_id, session_id)?;
        let (cols, rows) = self.current_effective_pane_dims();
        self.sessions[index]
            .session
            .set_split_ratio(pane_id, ratio_percent, cols, rows)?;
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        Ok(())
    }

    /// `agent.report` for the JSON-RPC API: store an externally reported
    /// agent state for one pane. The report writes into the same status
    /// store as manifest detection (so seen/done derivation, notifications
    /// and every display path behave identically) and suppresses detection
    /// for that pane for [`REPORTED_AGENT_TTL`], after which manifest
    /// detection resumes and overwrites it.
    pub fn api_report_agent(
        &mut self,
        pane_id: usize,
        session_id: Option<&str>,
        kind: String,
        state: crate::agent::AgentState,
    ) -> Result<(), String> {
        let session_index = self
            .sessions
            .iter()
            .position(|managed| {
                session_id.is_none_or(|filter| filter == managed.session_id)
                    && managed.session.pane_exists(pane_id)
            })
            .ok_or_else(|| "pane not found".to_string())?;

        let now = Instant::now();
        let viewing = session_index == self.view.active_session
            && self.sessions[session_index].session.focused_pane_id() == Some(pane_id);
        let notify_mode = self.agent_notify;
        let mut notifications = Vec::new();
        let managed = &mut self.sessions[session_index];
        let changed = Self::apply_agent_status(
            managed,
            pane_id,
            crate::agent::AgentStatus {
                kind,
                state,
                since: now,
            },
            viewing,
            notify_mode,
            &mut notifications,
        );
        managed.agents.reported.insert(pane_id, now);

        for message in notifications {
            self.broadcast_notification_to_clients(&message);
        }
        if changed {
            self.needs_render = true;
            self.push_agent_changed_events(vec![(session_index, pane_id)]);
        }
        Ok(())
    }

    /// Queue one API event for fan-out to subscribed API connections.
    /// The queue is bounded by [`API_EVENT_QUEUE_MAX`]: the oldest event is
    /// dropped (with a log line) when full. Every event also fans out to
    /// plugin `[[on_event]]` commands (no-op when no plugins are loaded).
    pub(crate) fn push_api_event(&mut self, name: &str, params: serde_json::Value) {
        if self.pending_api_events.len() >= API_EVENT_QUEUE_MAX {
            self.pending_api_events.remove(0);
            self.write_log("api event queue full; dropped oldest event");
        }
        let event = crate::api::ApiEvent {
            name: name.to_string(),
            params,
        };
        self.plugins.dispatch_event(&event);
        self.pending_api_events.push(event);
    }

    /// Drain the queued API events; called by the server loop each pass to
    /// fan them out to subscribed API connections.
    pub fn take_pending_api_events(&mut self) -> Vec<crate::api::ApiEvent> {
        std::mem::take(&mut self.pending_api_events)
    }
}
