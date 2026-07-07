//! CJK / IME affordances (herdr-inspired, `[ime]` config section).
//!
//! Two independent features live here:
//!
//! - **IME cursor anchoring**: macOS IMEs draw their candidate window at the
//!   host terminal's *real* cursor. The renderer already parks the host
//!   cursor at the focused pane's cursor cell each frame, but a guest that
//!   hides its cursor (DECTCEM, e.g. Claude Code drawing its own cursor)
//!   would leave the host cursor hidden and the candidate window stranded.
//!   `reveal_hidden_cursor` re-shows the host cursor at the guest cursor
//!   cell, optionally gated to panes running specific agents.
//! - **Prefix input-source switching**: with a CJK IME active, the key after
//!   the prefix can be eaten by the IME. `prefix_ascii_command` /
//!   `prefix_restore_command` run a user command (`im-select`, `macism`,
//!   `fcitx5-remote`, ...) when the pending-prefix state starts and ends.

use super::App;

impl App {
    /// Re-show a guest-hidden cursor at the focused pane's cursor cell when
    /// `[ime] reveal_hidden_cursor` applies, so IME candidate windows anchor
    /// to the true input position.
    pub(super) fn apply_ime_cursor_policy(&self, frame: &mut crate::session::manager::RenderFrame) {
        if !frame.focused_cursor_hidden || !self.ime.reveal_hidden_cursor {
            return;
        }
        if !self.ime.agents.is_empty() {
            let Some(kind) = self.focused_agent_kind() else {
                return;
            };
            if !self
                .ime
                .agents
                .iter()
                .any(|agent| agent.eq_ignore_ascii_case(&kind))
            {
                return;
            }
        }
        frame.focused_cursor_hidden = false;
        if let Some(shape) = self.ime.cursor_shape {
            frame.cursor_style = match shape {
                crate::config::ImeCursorShape::Block => {
                    crossterm::cursor::SetCursorStyle::SteadyBlock
                }
                crate::config::ImeCursorShape::Bar => crossterm::cursor::SetCursorStyle::SteadyBar,
                crate::config::ImeCursorShape::Underline => {
                    crossterm::cursor::SetCursorStyle::SteadyUnderScore
                }
            };
        }
    }

    /// Detected agent kind (manifest name, e.g. "claude") of the focused
    /// pane, if any.
    fn focused_agent_kind(&self) -> Option<String> {
        let managed = self.sessions.get(self.view.active_session)?;
        let pane_id = managed.session.focused_pane_id()?;
        let status = managed.agents.statuses.get(&pane_id)?;
        Some(status.kind.clone())
    }

    /// Run the `[ime]` input-source command matching the current
    /// pending-prefix state, once per transition. Called after every key so
    /// every path that arms or clears the prefix (bound command, Esc,
    /// pass-through, sticky exit) is covered.
    pub(super) fn sync_prefix_input_source(&mut self) {
        if self.ime.prefix_ascii_command.is_none() && self.ime.prefix_restore_command.is_none() {
            return;
        }
        let active = self.view.keys.prefix_active();
        if active == self.prefix_input_source_switched {
            return;
        }
        self.prefix_input_source_switched = active;
        let command = if active {
            self.ime.prefix_ascii_command.clone()
        } else {
            self.ime.prefix_restore_command.clone()
        };
        let Some(command) = command.map(|command| command.trim().to_string()) else {
            return;
        };
        if command.is_empty() {
            return;
        }
        let context = self.current_hook_context();
        self.spawn_shell_detached(
            "ime input source",
            command,
            context,
            vec![(
                "SPECTRA_PREFIX_PENDING".to_string(),
                if active { "1" } else { "0" }.to_string(),
            )],
        );
    }
}
