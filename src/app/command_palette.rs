use std::collections::HashMap;
use std::mem;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::input::CommandAction;
use crate::core_lib::filtering::fzf_style_match;

use super::types::*;
use super::{App, AppSignal};

impl App {
    pub(super) fn open_command_palette(&mut self) {
        self.view.input_mode = InputMode::CommandPalette {
            state: CommandPaletteState::default(),
        };
    }

    pub(super) fn handle_command_palette_mode_key(
        &mut self,
        mut state: CommandPaletteState,
        key: KeyEvent,
        signal: &mut AppSignal,
    ) -> InputMode {
        if key.kind != KeyEventKind::Press {
            return InputMode::CommandPalette { state };
        }

        let recent_command_ids = self.command_history.get_recent_commands(100);
        let entries = Self::command_palette_entries_for(self.view.locked_input);
        let mut candidates =
            Self::command_palette_candidates(&state, &entries, &recent_command_ids);
        Self::command_palette_clamp_selected(&mut state, candidates.len());

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Left => {
                    state.text_input.move_word_left();
                    return InputMode::CommandPalette { state };
                }
                KeyCode::Right => {
                    state.text_input.move_word_right();
                    return InputMode::CommandPalette { state };
                }
                KeyCode::Char(ch) => match ch.to_ascii_lowercase() {
                    'n' | 'j' => {
                        Self::command_palette_select_next(&mut state, candidates.len());
                        return InputMode::CommandPalette { state };
                    }
                    'p' => {
                        Self::command_palette_select_prev(&mut state, candidates.len());
                        return InputMode::CommandPalette { state };
                    }
                    'f' => {
                        state.text_input.move_right();
                        return InputMode::CommandPalette { state };
                    }
                    'b' => {
                        state.text_input.move_left();
                        return InputMode::CommandPalette { state };
                    }
                    'a' => {
                        state.text_input.move_start();
                        return InputMode::CommandPalette { state };
                    }
                    'e' => {
                        state.text_input.move_end();
                        return InputMode::CommandPalette { state };
                    }
                    'w' => {
                        if state.text_input.delete_prev_word() {
                            candidates = Self::command_palette_candidates(
                                &state,
                                &entries,
                                &recent_command_ids,
                            );
                            Self::command_palette_clamp_selected(&mut state, candidates.len());
                        }
                        return InputMode::CommandPalette { state };
                    }
                    'k' => {
                        if state.text_input.delete_to_end() {
                            candidates = Self::command_palette_candidates(
                                &state,
                                &entries,
                                &recent_command_ids,
                            );
                            Self::command_palette_clamp_selected(&mut state, candidates.len());
                        }
                        return InputMode::CommandPalette { state };
                    }
                    'u' => {
                        state.text_input.clear();
                        candidates =
                            Self::command_palette_candidates(&state, &entries, &recent_command_ids);
                        Self::command_palette_clamp_selected(&mut state, candidates.len());
                        return InputMode::CommandPalette { state };
                    }
                    'c' | 'q' => {
                        return InputMode::Normal;
                    }
                    _ => {
                        return InputMode::CommandPalette { state };
                    }
                },
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => InputMode::Normal,
            KeyCode::Backspace => {
                if state.text_input.backspace() {
                    candidates =
                        Self::command_palette_candidates(&state, &entries, &recent_command_ids);
                    Self::command_palette_clamp_selected(&mut state, candidates.len());
                }
                InputMode::CommandPalette { state }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.text_input.insert_char(ch);
                candidates =
                    Self::command_palette_candidates(&state, &entries, &recent_command_ids);
                Self::command_palette_clamp_selected(&mut state, candidates.len());
                InputMode::CommandPalette { state }
            }
            KeyCode::Left => {
                state.text_input.move_left();
                InputMode::CommandPalette { state }
            }
            KeyCode::Right => {
                state.text_input.move_right();
                InputMode::CommandPalette { state }
            }
            KeyCode::Up => {
                Self::command_palette_select_prev(&mut state, candidates.len());
                InputMode::CommandPalette { state }
            }
            KeyCode::Down => {
                Self::command_palette_select_next(&mut state, candidates.len());
                InputMode::CommandPalette { state }
            }
            KeyCode::Enter => {
                if let Some(candidate) = candidates.get(state.selected).copied() {
                    let entry = &entries[candidate.entry_index];
                    let _ = self.command_history.record_execution(&entry.id);
                    *signal = self.handle_action(entry.action);
                    let next_mode = mem::replace(&mut self.view.input_mode, InputMode::Normal);
                    match next_mode {
                        InputMode::CommandPalette { .. } => InputMode::Normal,
                        other => other,
                    }
                } else {
                    InputMode::CommandPalette { state }
                }
            }
            _ => InputMode::CommandPalette { state },
        }
    }

    pub(super) fn command_palette_clamp_selected(
        state: &mut CommandPaletteState,
        candidate_count: usize,
    ) {
        if candidate_count == 0 {
            state.selected = 0;
        } else {
            state.selected = state.selected.min(candidate_count.saturating_sub(1));
        }
    }

    fn command_palette_select_next(state: &mut CommandPaletteState, candidate_count: usize) {
        if candidate_count == 0 {
            state.selected = 0;
            return;
        }
        state.selected = if state.selected + 1 >= candidate_count {
            0
        } else {
            state.selected + 1
        };
    }

    fn command_palette_select_prev(state: &mut CommandPaletteState, candidate_count: usize) {
        if candidate_count == 0 {
            state.selected = 0;
            return;
        }
        state.selected = if state.selected == 0 {
            candidate_count - 1
        } else {
            state.selected - 1
        };
    }

    fn push_command_palette_entry(
        entries: &mut Vec<CommandPaletteEntry>,
        id: &str,
        action: CommandAction,
        label: &str,
        search_key: &str,
        preview_lines: &[&str],
    ) {
        entries.push(CommandPaletteEntry {
            id: id.to_string(),
            action,
            label: label.to_string(),
            search_key: search_key.to_string(),
            preview_lines: preview_lines
                .iter()
                .map(|line| (*line).to_string())
                .collect(),
        });
    }

    pub(super) fn command_palette_entries_for(locked_input: bool) -> Vec<CommandPaletteEntry> {
        let mut entries = Vec::new();

        Self::push_command_palette_entry(
            &mut entries,
            "pane.split.vertical",
            CommandAction::Split(crate::ui::window_manager::SplitAxis::Vertical),
            "Split pane vertical",
            "split pane vertical",
            &["action: split pane", "axis: vertical"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.split.horizontal",
            CommandAction::Split(crate::ui::window_manager::SplitAxis::Horizontal),
            "Split pane horizontal",
            "split pane horizontal",
            &["action: split pane", "axis: horizontal"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.left",
            CommandAction::Focus(crate::ui::window_manager::Direction::Left),
            "Focus pane left",
            "focus pane left",
            &["action: focus pane", "direction: left"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.down",
            CommandAction::Focus(crate::ui::window_manager::Direction::Down),
            "Focus pane down",
            "focus pane down",
            &["action: focus pane", "direction: down"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.up",
            CommandAction::Focus(crate::ui::window_manager::Direction::Up),
            "Focus pane up",
            "focus pane up",
            &["action: focus pane", "direction: up"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.right",
            CommandAction::Focus(crate::ui::window_manager::Direction::Right),
            "Focus pane right",
            "focus pane right",
            &["action: focus pane", "direction: right"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.next",
            CommandAction::FocusNextPane,
            "Focus next pane",
            "focus next pane previous buffer",
            &["action: focus pane", "direction: next in pane history"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.focus.prev",
            CommandAction::FocusPrevPane,
            "Focus previous pane",
            "focus previous pane prev pane",
            &["action: focus pane", "direction: previous in pane history"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.close",
            CommandAction::ClosePane,
            "Close focused pane",
            "close pane focused",
            &["action: close focused pane"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.new",
            CommandAction::NewWindow,
            "Create new window",
            "new window create",
            &["action: create new window"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.next",
            CommandAction::NextWindow,
            "Next window",
            "next window",
            &["action: focus next window"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.prev",
            CommandAction::PrevWindow,
            "Previous window",
            "previous prev window",
            &["action: focus previous window"],
        );
        for window in 1..=10 {
            entries.push(CommandPaletteEntry {
                id: format!("window.select.{window}"),
                action: CommandAction::SelectWindow(window),
                label: format!("Select window {window}"),
                search_key: format!("select window {window}"),
                preview_lines: vec![
                    "action: select window".to_string(),
                    format!("window: {window}"),
                ],
            });
        }
        Self::push_command_palette_entry(
            &mut entries,
            "pane.resize.left",
            CommandAction::Resize(crate::ui::window_manager::Direction::Left),
            "Resize pane left",
            "resize pane left",
            &["action: resize focused pane", "direction: left"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.resize.down",
            CommandAction::Resize(crate::ui::window_manager::Direction::Down),
            "Resize pane down",
            "resize pane down",
            &["action: resize focused pane", "direction: down"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.resize.up",
            CommandAction::Resize(crate::ui::window_manager::Direction::Up),
            "Resize pane up",
            "resize pane up",
            &["action: resize focused pane", "direction: up"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.resize.right",
            CommandAction::Resize(crate::ui::window_manager::Direction::Right),
            "Resize pane right",
            "resize pane right",
            &["action: resize focused pane", "direction: right"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.swap.prev",
            CommandAction::SwapPrevWindow,
            "Swap window with previous",
            "swap previous window",
            &["action: swap window", "direction: previous"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.swap.next",
            CommandAction::SwapNextWindow,
            "Swap window with next",
            "swap next window",
            &["action: swap window", "direction: next"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "tree.open",
            CommandAction::SystemTree,
            "Open system tree",
            "system tree window tree session list",
            &["action: open tree popup", "scope: sessions/windows/panes"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.side_tree.toggle",
            CommandAction::SideWindowTree,
            "Toggle side window tree",
            "toggle side window tree sidebar windows",
            &[
                "action: toggle side window list",
                "scope: active window (per-window state)",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.peek_all_windows",
            CommandAction::PeekAllWindows,
            "Peek all panes in session",
            "peek all panes windows session overview",
            &[
                "action: tile all session panes in one view",
                "exit: any key restores previous focus",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "cursor-mode.enter",
            CommandAction::EnterCursorMode,
            "Enter cursor mode",
            "enter cursor mode copy mode scrollback",
            &[
                "action: enter cursor mode on focused pane",
                "view: freeze current pane viewport + scrollback snapshot",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "cursor-mode.leave",
            CommandAction::LeaveCursorMode,
            "Leave cursor mode",
            "leave cursor mode exit copy mode",
            &["action: leave cursor mode", "scope: current client"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.rename",
            CommandAction::RenameSession,
            "Rename session",
            "rename session",
            &["action: rename active session"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.new",
            CommandAction::NewSession,
            "Create new session",
            "new session create",
            &["action: create and switch to a new session"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.next",
            CommandAction::NextSession,
            "Next session",
            "next session",
            &["action: switch to next session"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.prev",
            CommandAction::PrevSession,
            "Previous session",
            "previous prev session",
            &["action: switch to previous session"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.zoom.toggle",
            CommandAction::ToggleZoom,
            "Toggle zoom",
            "zoom toggle",
            &["action: toggle zoom for active window"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.sync.toggle",
            CommandAction::ToggleSynchronizePanes,
            "Toggle synchronize panes",
            "synchronize panes toggle",
            &["action: toggle synchronized input in active window"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "layout.save",
            CommandAction::SaveLayout,
            "Save layout snapshot",
            "save layout",
            &["action: save active session layout to disk"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "log.write",
            CommandAction::WriteLog,
            "Write log entry",
            "write log",
            &["action: append manual event to session log"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "scrollback.write",
            CommandAction::WriteScrollback,
            "Write pane scrollback",
            "write scrollback pane",
            &["action: dump focused pane scrollback to file"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "pane.buffer.open-editor",
            CommandAction::OpenPaneBufferInEditor,
            "Open current pane buffer in editor",
            "open current pane buffer in editor write scrollback",
            &[
                "action: open current pane buffer in editor",
                "scope: new window in current session",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "config.reload",
            CommandAction::ReloadConfig,
            "Reload config",
            "reload config source file",
            &["action: reload spectra config from disk"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "config.create_default",
            CommandAction::CreateDefaultConfig,
            "Create default config",
            "create default config source file",
            &["action: create spectra config with defaults"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "config.open-editor",
            CommandAction::OpenConfigInEditor,
            "Open config in editor",
            "open config in editor edit config file",
            &[
                "action: open config file in editor",
                "scope: new window in current session",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "client.detach",
            CommandAction::DetachClient,
            "Detach client",
            "detach client",
            &["action: detach current client from server"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "app.quit",
            CommandAction::Quit,
            "Quit spectra",
            "quit",
            &["action: quit spectra server/session"],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "session.kill",
            CommandAction::KillSession,
            "Kill current session",
            "kill current session close destroy remove",
            &[
                "action: kill the active session",
                "scope: all windows and panes in session",
            ],
        );
        Self::push_command_palette_entry(
            &mut entries,
            "window.close",
            CommandAction::CloseWindow,
            "Close current window",
            "close current window kill remove destroy",
            &[
                "action: close the active window",
                "scope: all panes in window",
            ],
        );

        if locked_input {
            Self::push_command_palette_entry(
                &mut entries,
                "client.leave_lock_mode",
                CommandAction::LeaveLockMode,
                "Leave lock mode",
                "leave lock mode unlock",
                &["action: leave lock mode", "scope: current client"],
            );
        } else {
            Self::push_command_palette_entry(
                &mut entries,
                "client.enter_lock_mode",
                CommandAction::EnterLockMode,
                "Enter lock mode",
                "enter lock mode lock",
                &[
                    "action: enter lock mode",
                    "scope: current client",
                    "all keys forwarded to pane",
                ],
            );
        }

        entries
    }

    #[cfg(test)]
    pub(super) fn command_palette_entries() -> Vec<CommandPaletteEntry> {
        Self::command_palette_entries_for(false)
    }

    pub(super) fn command_palette_candidates(
        state: &CommandPaletteState,
        entries: &[CommandPaletteEntry],
        recent_command_ids: &[String],
    ) -> Vec<ScoredCommandCandidate> {
        let query = state.text_input.text.trim();
        if query.is_empty() {
            let recent_command_rank = recent_command_ids
                .iter()
                .enumerate()
                .map(|(rank, id)| (id.as_str(), rank))
                .collect::<HashMap<_, _>>();

            let mut candidates = entries
                .iter()
                .enumerate()
                .map(|(entry_index, _)| ScoredCommandCandidate {
                    entry_index,
                    score: 0,
                })
                .collect::<Vec<_>>();

            candidates.sort_by(|a, b| {
                let a_entry = &entries[a.entry_index];
                let b_entry = &entries[b.entry_index];
                let a_rank = recent_command_rank.get(a_entry.id.as_str());
                let b_rank = recent_command_rank.get(b_entry.id.as_str());

                match (a_rank, b_rank) {
                    (Some(left), Some(right)) => left.cmp(right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a_entry.label.cmp(&b_entry.label),
                }
            });
            return candidates;
        }

        let mut candidates = entries
            .iter()
            .enumerate()
            .filter_map(|(entry_index, entry)| {
                fzf_style_match(&entry.search_key, query)
                    .map(|(score, _)| ScoredCommandCandidate { entry_index, score })
            })
            .collect::<Vec<_>>();

        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry_index.cmp(&b.entry_index))
        });
        candidates
    }
}
