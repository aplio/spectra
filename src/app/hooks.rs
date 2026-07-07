use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use super::App;
use super::types::*;

impl App {
    pub(super) fn hook_command(&self, event: HookEvent) -> Option<&str> {
        match event {
            HookEvent::SessionCreated => self.hooks.session_created.as_deref(),
            HookEvent::SessionKilled => self.hooks.session_killed.as_deref(),
            HookEvent::WindowCreated => self.hooks.window_created.as_deref(),
            HookEvent::PaneSplit => self.hooks.pane_split.as_deref(),
            HookEvent::PaneClosed => self.hooks.pane_closed.as_deref(),
            HookEvent::PaneRestored => self.hooks.pane_restored.as_deref(),
            HookEvent::ConfigReloaded => self.hooks.config_reloaded.as_deref(),
        }
    }

    pub(super) fn current_hook_context(&self) -> HookContext {
        let session = self.current_session();
        HookContext {
            session_id: Some(self.current_session_id().to_string()),
            session_name: Some(session.session_name().to_string()),
            window_id: session.focused_window_id(),
            window_number: session.focused_window_number(),
            pane_id: session.focused_pane_id(),
        }
    }

    pub(super) fn emit_hook(&mut self, event: HookEvent, context: HookContext) {
        // Bridge every hook emission to the JSON-RPC API event queue with the
        // same context fields, independent of whether a hook command is set.
        self.push_api_event(event.api_event_name(), context.api_event_params());

        let Some(command) = self.hook_command(event).map(str::trim) else {
            return;
        };
        if command.is_empty() {
            return;
        }

        let event_name = event.as_str();
        self.spawn_shell_detached(
            &format!("hook {event_name}"),
            command.to_string(),
            context,
            vec![("SPECTRA_HOOK_EVENT".to_string(), event_name.to_string())],
        );
    }

    /// Execute a `run:` key-binding command using the same fire-and-forget
    /// shell machinery as hooks (`/bin/sh -lc`, detached thread, SPECTRA_*
    /// env context, output to the session log on failure).
    pub(super) fn run_shell_binding(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        let context = self.current_hook_context();
        self.spawn_shell_detached("run binding", command.to_string(), context, Vec::new());
    }

    fn spawn_shell_detached(
        &mut self,
        label: &str,
        command: String,
        context: HookContext,
        mut envs: Vec<(String, String)>,
    ) {
        let log_session_id = context
            .session_id
            .clone()
            .or_else(|| {
                self.sessions
                    .first()
                    .map(|session| session.session_id.clone())
            })
            .unwrap_or_else(|| "global".to_string());
        let store = self.store.clone();

        if let Some(session_id) = context.session_id {
            envs.push(("SPECTRA_SESSION_ID".to_string(), session_id));
        }
        if let Some(session_name) = context.session_name {
            envs.push(("SPECTRA_SESSION_NAME".to_string(), session_name));
        }
        if let Some(window_id) = context.window_id {
            envs.push(("SPECTRA_WINDOW_ID".to_string(), window_id.to_string()));
        }
        if let Some(window_number) = context.window_number {
            envs.push((
                "SPECTRA_WINDOW_NUMBER".to_string(),
                window_number.to_string(),
            ));
        }
        if let Some(pane_id) = context.pane_id {
            envs.push(("SPECTRA_PANE_ID".to_string(), pane_id.to_string()));
        }

        let thread_label = label.replace(' ', "-");
        let log_label = label.to_string();
        let spawn_result = thread::Builder::new()
            .name(format!("spectra-{thread_label}"))
            .spawn(move || {
                let status = Command::new("/bin/sh")
                    .arg("-lc")
                    .arg(&command)
                    .envs(envs)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();

                match status {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        let _ = store.append_log_line(
                            &log_session_id,
                            &format!("{log_label} failed: {status}"),
                        );
                    }
                    Err(err) => {
                        let _ = store.append_log_line(
                            &log_session_id,
                            &format!("{log_label} failed to spawn: {err}"),
                        );
                    }
                }
            });

        if let Err(err) = spawn_result {
            self.set_message(
                &format!("{label} spawn failed: {err}"),
                Duration::from_secs(3),
            );
            self.write_log(&format!("{label} spawn failed: {err}"));
        }
    }
}
