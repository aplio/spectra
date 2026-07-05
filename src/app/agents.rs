use super::*;

impl App {
    /// Queue `agent.changed` events for panes whose agent display state
    /// changed (`(session_index, pane_id)` pairs). Panes without a stored
    /// agent status (e.g. detection lost) are skipped.
    pub(super) fn push_agent_changed_events(&mut self, changed_panes: Vec<(usize, usize)>) {
        let mut events = Vec::new();
        for (session_index, pane_id) in changed_panes {
            let Some(managed) = self.sessions.get(session_index) else {
                continue;
            };
            let Some(status) = managed.agents.statuses.get(&pane_id) else {
                continue;
            };
            let Some(state) = managed.agents.display_state(pane_id) else {
                continue;
            };
            events.push(serde_json::json!({
                "pane_id": pane_id,
                "session_id": managed.session_id,
                "kind": status.kind,
                "state": state.as_str(),
            }));
        }
        for params in events {
            self.push_api_event("agent.changed", params);
        }
    }

    /// Re-run AI-agent detection for panes whose output changed, throttled to
    /// at most once per [`AGENT_DETECT_INTERVAL`] per pane. Throttled panes
    /// stay pending and are picked up by a later tick. Panes with a fresh
    /// external `agent.report` are skipped (and stay pending) until the
    /// report expires. Returns true when any pane's stored agent status
    /// changed.
    pub(super) fn run_agent_detection(
        &mut self,
        dirty_by_session: Vec<(usize, Vec<usize>)>,
        now: Instant,
    ) -> bool {
        for (session_index, pane_ids) in dirty_by_session {
            if let Some(managed) = self.sessions.get_mut(session_index) {
                managed.agents.pending.extend(pane_ids);
            }
        }

        let mut changed = false;
        let mut changed_panes = Vec::new();
        let mut notifications = Vec::new();
        let notify_mode = self.agent_notify;
        let active_session = self.view.active_session;
        let manifests = std::sync::Arc::clone(&self.agent_manifests);
        for (session_index, managed) in self.sessions.iter_mut().enumerate() {
            if !managed.agents.statuses.is_empty()
                || !managed.agents.last_run.is_empty()
                || !managed.agents.pending.is_empty()
            {
                let session = &managed.session;
                changed |= managed
                    .agents
                    .prune_closed_panes(|pane_id| session.pane_exists(pane_id));
            }
            if managed.agents.pending.is_empty() {
                continue;
            }
            let due: Vec<usize> =
                managed
                    .agents
                    .pending
                    .iter()
                    .copied()
                    .filter(|pane_id| {
                        !managed.agents.report_fresh(*pane_id, now)
                            && managed.agents.last_run.get(pane_id).is_none_or(|last| {
                                now.duration_since(*last) >= AGENT_DETECT_INTERVAL
                            })
                    })
                    .collect();
            let focused_pane = managed.session.focused_pane_id();
            for pane_id in due {
                managed.agents.pending.remove(&pane_id);
                managed.agents.last_run.insert(pane_id, now);
                let viewing = session_index == active_session && focused_pane == Some(pane_id);
                if Self::detect_agent_for_pane(
                    managed,
                    &manifests,
                    pane_id,
                    now,
                    viewing,
                    notify_mode,
                    &mut notifications,
                ) {
                    changed = true;
                    changed_panes.push((session_index, pane_id));
                }
            }
        }
        for message in notifications {
            self.broadcast_notification_to_clients(&message);
        }
        self.push_agent_changed_events(changed_panes);
        changed
    }

    /// Run manifest detection for one pane and update its stored status.
    /// `viewing` = the pane is the focused pane of the active window of the
    /// active session, i.e. the user is looking at it right now; it feeds the
    /// seen flag that derives "done". Returns true when the stored status
    /// changed.
    fn detect_agent_for_pane(
        managed: &mut ManagedSession,
        manifests: &[crate::agent::AgentManifest],
        pane_id: usize,
        now: Instant,
        viewing: bool,
        notify_mode: config::AgentNotifyMode,
        notifications: &mut Vec<String>,
    ) -> bool {
        let Some(screen_lines) = managed.session.pane_screen_lines(pane_id) else {
            managed.agents.seen.remove(&pane_id);
            managed.agents.notified.remove(&pane_id);
            return managed.agents.statuses.remove(&pane_id).is_some();
        };
        let foreground_process = managed
            .session
            .pane_child_pid(pane_id)
            .and_then(crate::agent::foreground_process_name);
        let snapshot = crate::agent::PaneSnapshot {
            screen_lines: &screen_lines,
            osc_title: managed.terminal_titles.get(&pane_id).map(String::as_str),
            foreground_process: foreground_process.as_deref(),
        };

        match crate::agent::detect(manifests, &snapshot) {
            Some((kind, state)) => Self::apply_agent_status(
                managed,
                pane_id,
                crate::agent::AgentStatus {
                    kind,
                    state,
                    since: now,
                },
                viewing,
                notify_mode,
                notifications,
            ),
            None => {
                managed.agents.seen.remove(&pane_id);
                managed.agents.notified.remove(&pane_id);
                managed.agents.statuses.remove(&pane_id).is_some()
            }
        }
    }

    /// Store one pane's agent status (from manifest detection or an external
    /// `agent.report`), updating seen/done and notification bookkeeping via
    /// the same transitions in both cases. Returns true when the stored
    /// status changed.
    pub(super) fn apply_agent_status(
        managed: &mut ManagedSession,
        pane_id: usize,
        next: crate::agent::AgentStatus,
        viewing: bool,
        notify_mode: config::AgentNotifyMode,
        notifications: &mut Vec<String>,
    ) -> bool {
        let kind = next.kind.clone();
        let state = next.state;
        let previous = match managed.agents.statuses.get_mut(&pane_id) {
            Some(status) if status.kind == next.kind && status.state == next.state => {
                return false;
            }
            Some(status) => {
                let previous = status.state;
                *status = next;
                Some(previous)
            }
            None => {
                managed.agents.statuses.insert(pane_id, next);
                None
            }
        };
        managed
            .agents
            .note_status_change(pane_id, previous, state, viewing);
        if let Some(display) =
            managed
                .agents
                .notifiable_transition(pane_id, previous, state, viewing, notify_mode)
        {
            notifications.push(Self::agent_notification_message(&kind, display, pane_id));
        }
        true
    }

    /// Human-readable notification message for an agent state change, e.g.
    /// `spectra: claude blocked (pane 3)`.
    fn agent_notification_message(
        kind: &str,
        display: crate::agent::AgentDisplayState,
        pane_id: usize,
    ) -> String {
        format!("spectra: {kind} {} (pane {pane_id})", display.as_str())
    }

    /// Mark the focused pane of the active window of the active session as
    /// seen. Called while rendering, so a pane the user is looking at never
    /// shows "done". Returns true when the flag was newly set. A newly seen
    /// idle pane transitions done → idle, so that also queues an
    /// `agent.changed` API event.
    pub(super) fn mark_focused_agent_seen(&mut self) -> bool {
        let session_index = self.view.active_session;
        let Some(managed) = self.sessions.get_mut(session_index) else {
            return false;
        };
        let Some(pane_id) = managed.session.focused_pane_id() else {
            return false;
        };
        let newly_seen =
            managed.agents.statuses.contains_key(&pane_id) && managed.agents.seen.insert(pane_id);
        let display_changed = newly_seen
            && managed
                .agents
                .statuses
                .get(&pane_id)
                .is_some_and(|status| status.state == crate::agent::AgentState::Idle);
        if display_changed {
            self.push_agent_changed_events(vec![(session_index, pane_id)]);
        }
        newly_seen
    }

    /// `{agent}` status token for the focused pane of the active session,
    /// e.g. `claude:working`; empty when no agent is detected. The state is
    /// the derived display state, so "done" is possible.
    pub(super) fn focused_agent_token(&self) -> String {
        self.sessions
            .get(self.view.active_session)
            .and_then(|managed| {
                let pane_id = managed.session.focused_pane_id()?;
                let status = managed.agents.statuses.get(&pane_id)?;
                let state = managed.agents.display_state(pane_id)?;
                Some(format!("{}:{}", status.kind, state.as_str()))
            })
            .unwrap_or_default()
    }
}
