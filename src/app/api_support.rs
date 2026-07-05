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
    pub fn api_split_pane(
        &mut self,
        pane_id: Option<usize>,
        session_id: Option<&str>,
        axis: crate::ui::window_manager::SplitAxis,
    ) -> Result<usize, String> {
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
            self.current_session_mut()
                .focus_pane_id(pane_id)
                .map_err(|err| format!("split-window failed: {err}"))?;
        }

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
