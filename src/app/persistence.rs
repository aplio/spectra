use std::time::Duration;

use crate::config;
use crate::storage::{SessionInfo, unix_time_now};

use super::App;
use super::types::*;

fn resolve_editor_command(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| Some("vi".to_string()))
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

impl App {
    pub(super) fn save_active_layout(&mut self) {
        let snapshot = self
            .current_session()
            .layout_snapshot(self.view.cols, self.view.rows);
        match self
            .store
            .write_layout(self.current_session_id(), &snapshot)
        {
            Ok(path) => {
                self.set_message(
                    &format!("layout saved: {}", path.display()),
                    Duration::from_secs(3),
                );
                self.write_log("layout saved");
            }
            Err(err) => {
                self.set_message(
                    &format!("layout save failed: {err}"),
                    Duration::from_secs(3),
                );
            }
        }
    }

    pub(super) fn write_active_scrollback(&mut self) {
        let focused = self.current_session().focused_pane_id();
        let Some(scrollback) = self.current_session().focused_scrollback_text() else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };
        let Some(pane_id) = focused else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };

        match self
            .store
            .write_scrollback(self.current_session_id(), pane_id, &scrollback)
        {
            Ok(path) => {
                self.set_message(
                    &format!("scrollback written: {}", path.display()),
                    Duration::from_secs(3),
                );
                self.write_log("scrollback written");
            }
            Err(err) => {
                self.set_message(
                    &format!("scrollback write failed: {err}"),
                    Duration::from_secs(3),
                );
            }
        }
    }

    pub(super) fn open_current_pane_buffer_in_editor(&mut self) {
        let Some(editor_command) = resolve_editor_command(self.editor_command.as_deref()) else {
            self.set_message(
                "no editor configured (set editor in config or $EDITOR)",
                Duration::from_secs(3),
            );
            return;
        };

        let Some(source_pane_id) = self.current_session().focused_pane_id() else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };
        let Some(scrollback) = self.current_session().focused_scrollback_text() else {
            self.set_message("no focused pane", Duration::from_secs(2));
            return;
        };
        let session_id = self.current_session_id().to_string();

        let path = match self
            .store
            .write_scrollback(&session_id, source_pane_id, &scrollback)
        {
            Ok(path) => path,
            Err(err) => {
                self.set_message(
                    &format!("scrollback write failed: {err}"),
                    Duration::from_secs(3),
                );
                return;
            }
        };

        let command_line = format!(
            "{editor_command} {}",
            shell_quote(path.to_string_lossy().as_ref())
        );
        let (cols, rows) = self.current_effective_pane_dims();
        if let Err(err) =
            self.current_session_mut()
                .new_window_with_command(cols, rows, vec![command_line])
        {
            self.set_message(
                &format!("open in editor failed: {err}"),
                Duration::from_secs(3),
            );
            return;
        }

        let Some(editor_pane_id) = self.current_session().focused_pane_id() else {
            self.set_message("editor pane focus failed", Duration::from_secs(3));
            return;
        };

        let target = EditorPaneCloseTarget {
            session_id,
            pane_id: editor_pane_id,
        };
        if !self.editor_pane_close_targets.contains(&target) {
            self.editor_pane_close_targets.push(target);
        }

        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::WindowCreated, self.current_hook_context());
        self.write_log("opened current pane buffer in editor");
        self.set_message(
            &format!("opened pane buffer in editor: {}", path.display()),
            Duration::from_secs(3),
        );
    }

    pub(super) fn open_config_in_editor(&mut self) {
        let Some(editor_command) = resolve_editor_command(self.editor_command.as_deref()) else {
            self.set_message(
                "no editor configured (set editor in config or $EDITOR)",
                Duration::from_secs(3),
            );
            return;
        };

        let path = config::config_path();
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            self.set_message(
                &format!("config dir create failed: {err}"),
                Duration::from_secs(3),
            );
            return;
        }

        let command_line = format!(
            "{editor_command} {}",
            shell_quote(path.to_string_lossy().as_ref())
        );
        let (cols, rows) = self.current_effective_pane_dims();
        if let Err(err) =
            self.current_session_mut()
                .new_window_with_command(cols, rows, vec![command_line])
        {
            self.set_message(
                &format!("open config in editor failed: {err}"),
                Duration::from_secs(3),
            );
            return;
        }

        let Some(editor_pane_id) = self.current_session().focused_pane_id() else {
            self.set_message("editor pane focus failed", Duration::from_secs(3));
            return;
        };

        let session_id = self.current_session_id().to_string();
        let target = EditorPaneCloseTarget {
            session_id,
            pane_id: editor_pane_id,
        };
        if !self.editor_pane_close_targets.contains(&target) {
            self.editor_pane_close_targets.push(target);
        }

        self.sync_tree_names();
        self.needs_render = true;
        self.needs_full_clear = true;
        self.persist_active_session_info();
        self.emit_hook(HookEvent::WindowCreated, self.current_hook_context());
        self.write_log("opened config in editor");
        self.set_message(
            &format!("opened config in editor: {}", path.display()),
            Duration::from_secs(3),
        );
    }

    pub(super) fn close_exited_editor_panes(&mut self) {
        if self.editor_pane_close_targets.is_empty() {
            return;
        }

        let targets = std::mem::take(&mut self.editor_pane_close_targets);
        let mut remaining = Vec::with_capacity(targets.len());

        for target in targets {
            let Some(session_index) = self
                .sessions
                .iter()
                .position(|managed| managed.session_id == target.session_id)
            else {
                continue;
            };

            let session = &mut self.sessions[session_index].session;
            if !session.pane_exists(target.pane_id) {
                continue;
            }
            if !session.pane_closed(target.pane_id) {
                remaining.push(target);
                continue;
            }

            let close_result = {
                let (cols, rows) = self.current_effective_pane_dims();
                self.sessions[session_index]
                    .session
                    .close_pane(target.pane_id, cols, rows)
            };
            match close_result {
                Ok(()) => {
                    self.sync_tree_names();
                    self.needs_render = true;
                    self.needs_full_clear = true;
                    self.persist_active_session_info();
                    self.write_log("editor pane closed after command exit");
                }
                Err(err) => {
                    self.write_log(&format!(
                        "editor pane close failed for p{}: {err}",
                        target.pane_id
                    ));
                }
            }
        }

        self.editor_pane_close_targets = remaining;
    }

    pub(super) fn runtime_state_snapshot(&self) -> AppRuntimeState {
        AppRuntimeState {
            version: RUNTIME_STATE_VERSION,
            active_session: self.view.active_session,
            next_session_ordinal: self.next_session_ordinal,
            sessions: self
                .sessions
                .iter()
                .map(|managed| SessionRuntimeState {
                    ordinal: managed.ordinal,
                    session_id: managed.session_id.clone(),
                    session: managed.session.runtime_snapshot(),
                    window_names: managed.window_names.clone(),
                    pane_names: managed.pane_names.clone(),
                })
                .collect(),
            client_focus_profiles: self.collect_client_focus_profiles(),
        }
    }

    pub(super) fn persist_runtime_state(&mut self) {
        self.capture_active_client_focus_profile();
        let snapshot = self.runtime_state_snapshot();
        self.client_focus_profiles = snapshot.client_focus_profiles.clone();
        if let Err(err) = self.store.write_runtime_state(&snapshot) {
            self.set_message(
                &format!("runtime state write failed: {err}"),
                Duration::from_secs(3),
            );
        }
    }

    pub(super) fn persist_active_session_info(&mut self) {
        self.capture_active_client_focus_profile();
        let session = self.current_session();
        let info = SessionInfo {
            session_id: self.current_session_id().to_string(),
            session_name: session.session_name().to_string(),
            pid: std::process::id(),
            started_unix: self.started_unix,
            pane_count: session.pane_count(),
            window_count: session.window_count(),
            focused_pane_id: session.focused_pane_id(),
        };

        if let Err(err) = self.store.write_session_info(&info) {
            self.set_message(
                &format!("session info write failed: {err}"),
                Duration::from_secs(3),
            );
        }
        self.persist_runtime_state();
    }

    pub(super) fn write_log(&mut self, event: &str) {
        let line = format!("{} | {}", unix_time_now(), event);
        if let Err(err) = self.store.append_log_line(self.current_session_id(), &line) {
            self.set_message(&format!("log write failed: {err}"), Duration::from_secs(3));
        }
    }
}
