use crate::attach_target::AttachTarget;
use crate::ipc::protocol::{CommandRequest, CommandResult, CommandSplitAxis, SessionListEntry};

use super::App;
use super::types::*;

impl App {
    pub fn execute_command(&mut self, request: CommandRequest) -> Result<CommandResult, String> {
        match request {
            CommandRequest::NewSession => {
                let session_id = self.create_session_internal()?;
                Ok(CommandResult::Message {
                    message: format!("session created: {session_id}"),
                })
            }
            CommandRequest::Ls => Ok(CommandResult::SessionList {
                sessions: self.session_list_entries(),
            }),
            CommandRequest::KillSession { target } => {
                let session_index = match target.as_deref() {
                    Some(token) => self.resolve_session_index_for_attach(token)?,
                    None => self.view.active_session,
                };
                let session_id = self.sessions[session_index].session_id.clone();
                let shutdown = self.kill_session_by_index(session_index)?;
                let message = if shutdown {
                    format!("session killed: {session_id}; server shutting down")
                } else {
                    format!("session killed: {session_id}")
                };
                Ok(CommandResult::Message { message })
            }
            CommandRequest::NewWindow { target } => {
                if let Some(target) = target {
                    self.apply_attach_target(&target)?;
                }
                let (cols, rows) = self.current_effective_pane_dims();
                self.current_session_mut()
                    .new_window(cols, rows)
                    .map_err(|err| format!("new-window failed: {err}"))?;
                self.record_focus_for_active_session();
                self.sync_tree_names();
                self.needs_render = true;
                self.needs_full_clear = true;
                self.persist_active_session_info();
                self.emit_hook(HookEvent::WindowCreated, self.current_hook_context());
                Ok(CommandResult::Message {
                    message: "window created".to_string(),
                })
            }
            CommandRequest::SplitWindow { target, axis } => {
                if let Some(target) = target {
                    self.apply_attach_target(&target)?;
                }
                let split_axis = match axis {
                    CommandSplitAxis::Vertical => crate::ui::window_manager::SplitAxis::Vertical,
                    CommandSplitAxis::Horizontal => {
                        crate::ui::window_manager::SplitAxis::Horizontal
                    }
                };
                let (cols, rows) = self.current_effective_pane_dims();
                self.current_session_mut()
                    .split_focused(split_axis, cols, rows)
                    .map_err(|err| format!("split-window failed: {err}"))?;
                self.record_focus_for_active_session();
                self.sync_tree_names();
                self.needs_render = true;
                self.needs_full_clear = true;
                self.persist_active_session_info();
                self.emit_hook(HookEvent::PaneSplit, self.current_hook_context());
                Ok(CommandResult::Message {
                    message: "window split".to_string(),
                })
            }
            CommandRequest::SelectSession { target } => {
                if let Some(token) = target.as_deref() {
                    let index = self.resolve_session_index_for_attach(token)?;
                    self.select_session(index);
                    let session_id = self.current_session_id().to_string();
                    Ok(CommandResult::Message {
                        message: format!("session selected: {session_id}"),
                    })
                } else {
                    let session_id = self.current_session_id().to_string();
                    Ok(CommandResult::Message {
                        message: format!("session selected: {session_id}"),
                    })
                }
            }
            CommandRequest::SelectWindow { target, window } => {
                if window == 0 {
                    return Err("window must be >= 1".to_string());
                }
                if let Some(token) = target.as_deref() {
                    let index = self.resolve_session_index_for_attach(token)?;
                    self.select_session(index);
                }
                self.current_session_mut()
                    .focus_window_number(window)
                    .map_err(|err| format!("select-window failed: {err}"))?;
                self.record_focus_for_active_session();
                self.needs_render = true;
                self.needs_full_clear = true;
                self.persist_active_session_info();
                Ok(CommandResult::Message {
                    message: format!("window selected: w{window}"),
                })
            }
            CommandRequest::SelectPane { target, pane } => {
                if pane == 0 {
                    return Err("pane must be >= 1".to_string());
                }
                if let Some(token) = target.as_deref() {
                    let index = self.resolve_session_index_for_attach(token)?;
                    self.select_session(index);
                }
                self.current_session_mut()
                    .focus_pane_id(pane)
                    .map_err(|err| format!("select-pane failed: {err}"))?;
                self.record_focus_for_active_session();
                self.needs_render = true;
                self.needs_full_clear = true;
                self.persist_active_session_info();
                Ok(CommandResult::Message {
                    message: format!("pane selected: p{pane}"),
                })
            }
            CommandRequest::SendKeys { target, all, text } => {
                if text.is_empty() {
                    return Err("send-keys text cannot be empty".to_string());
                }
                let targets = self.resolve_send_keys_target(target.as_ref(), all)?;
                let bytes = text.as_bytes();
                for (session_index, pane_id) in targets.iter().copied() {
                    self.sessions[session_index]
                        .session
                        .send_to_pane(pane_id, bytes)
                        .map_err(|err| format!("send-keys failed: {err}"))?;
                }
                self.needs_render = true;
                Ok(CommandResult::Message {
                    message: format!("keys sent to {} pane(s)", targets.len()),
                })
            }
            CommandRequest::SourceFile { path } => {
                let message = self.reload_config_from_path(path.as_deref())?;
                Ok(CommandResult::Message { message })
            }
        }
    }

    fn session_list_entries(&self) -> Vec<SessionListEntry> {
        self.sessions
            .iter()
            .enumerate()
            .map(|(index, managed)| SessionListEntry {
                alias: format!("s{}", index + 1),
                session_id: managed.session_id.clone(),
                session_name: managed.session.session_name().to_string(),
                window_count: managed.session.window_count(),
                pane_count: managed.session.pane_count(),
                focused_window: managed.session.focused_window_number(),
                focused_pane: managed.session.focused_pane_id(),
                active: index == self.view.active_session,
            })
            .collect()
    }

    pub(super) fn resolve_session_index_for_attach(&self, token: &str) -> Result<usize, String> {
        if let Some((index, _)) = self
            .sessions
            .iter()
            .enumerate()
            .find(|(_, managed)| managed.session_id == token)
        {
            return Ok(index);
        }

        if let Some(alias_number) = super::parse_session_alias(token) {
            let alias_index = alias_number.saturating_sub(1);
            if alias_index < self.sessions.len() {
                return Ok(alias_index);
            }
            return Err(format!("session alias `{token}` not found"));
        }

        let mut by_name = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, managed)| {
                (managed.session.session_name() == token).then_some(index)
            });
        let Some(index) = by_name.next() else {
            return Err(format!("session `{token}` not found"));
        };
        if by_name.next().is_some() {
            return Err(format!("session name `{token}` is ambiguous"));
        }
        Ok(index)
    }

    fn resolve_send_keys_target(
        &self,
        target: Option<&AttachTarget>,
        all: bool,
    ) -> Result<Vec<(usize, usize)>, String> {
        if all && target.is_some() {
            return Err("--target cannot be used with --all".to_string());
        }

        if all {
            let mut targets = Vec::new();
            for (session_index, managed) in self.sessions.iter().enumerate() {
                targets.extend(
                    managed
                        .session
                        .all_pane_ids()
                        .into_iter()
                        .map(|pane_id| (session_index, pane_id)),
                );
            }
            return Ok(targets);
        }

        if let Some(target) = target {
            return self.resolve_send_keys_attach_target(target);
        }

        let pane_id = self
            .current_session()
            .focused_pane_id()
            .ok_or_else(|| "no focused pane".to_string())?;
        Ok(vec![(self.view.active_session, pane_id)])
    }

    fn resolve_send_keys_attach_target(
        &self,
        target: &AttachTarget,
    ) -> Result<Vec<(usize, usize)>, String> {
        let session_index = self.resolve_session_index_for_attach(&target.session_token)?;
        let window_entries = self.sessions[session_index].session.window_entries();

        self.ensure_target_window_exists(target, &window_entries)?;

        if let Some(pane_id) = self.resolve_target_pane_id(target, &window_entries)? {
            return Ok(vec![(session_index, pane_id)]);
        }

        if let Some(window_number) = target.window {
            let Some(pane_ids) = self.sessions[session_index]
                .session
                .pane_ids_for_window_number(window_number)
            else {
                return Err(format!(
                    "window w{window_number} not found in session `{}`",
                    target.session_token
                ));
            };
            return Ok(pane_ids
                .into_iter()
                .map(|pane_id| (session_index, pane_id))
                .collect());
        }

        Ok(window_entries
            .into_iter()
            .flat_map(|entry| {
                entry
                    .pane_ids
                    .into_iter()
                    .map(move |pane_id| (session_index, pane_id))
            })
            .collect())
    }

    pub(super) fn select_session(&mut self, index: usize) {
        self.view.active_session = index;
        self.restore_focus_for_active_session_from_history();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
    }
}
