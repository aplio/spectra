use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::filtering::fzf_style_match;

use super::App;
use super::types::*;

impl App {
    pub(super) fn open_keybindings(&mut self) {
        let rows = self
            .view
            .keys
            .keybinding_help_rows()
            .into_iter()
            .map(|(keys, description)| KeybindingRow { keys, description })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            self.set_message("no keybindings to show", Duration::from_secs(2));
            return;
        }
        self.view.input_mode = InputMode::Keybindings {
            state: KeybindingsState {
                rows,
                ..KeybindingsState::default()
            },
        };
        self.needs_render = true;
    }

    /// Indices into `state.rows` that match the current filter, best match
    /// first. With an empty filter every row is returned in its natural order.
    pub(super) fn keybinding_candidates(state: &KeybindingsState) -> Vec<usize> {
        let query = state.query_input.text.trim();
        if query.is_empty() {
            return (0..state.rows.len()).collect();
        }

        let mut scored = state
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let haystack = format!("{} {}", row.keys, row.description);
                fzf_style_match(&haystack, query).map(|(score, _)| (index, score))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().map(|(index, _)| index).collect()
    }

    fn keybindings_clamp_selected(state: &mut KeybindingsState, candidate_count: usize) {
        if candidate_count == 0 {
            state.selected = 0;
        } else if state.selected >= candidate_count {
            state.selected = candidate_count - 1;
        }
    }

    pub(super) fn handle_keybindings_mode_key(
        &mut self,
        mut state: KeybindingsState,
        key: KeyEvent,
    ) -> InputMode {
        let candidate_count = Self::keybinding_candidates(&state).len();
        Self::keybindings_clamp_selected(&mut state, candidate_count);

        // Ctrl-based navigation works regardless of filter focus.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n') | KeyCode::Char('j') => {
                    state.query_active = false;
                    Self::keybindings_move_down(&mut state, candidate_count);
                    return InputMode::Keybindings { state };
                }
                KeyCode::Char('p') => {
                    state.query_active = false;
                    Self::keybindings_move_up(&mut state);
                    return InputMode::Keybindings { state };
                }
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    return InputMode::Normal;
                }
                _ => {}
            }
        }

        if state.query_active {
            match key.code {
                KeyCode::Esc => {
                    state.query_active = false;
                }
                KeyCode::Enter => {
                    state.query_active = false;
                }
                KeyCode::Backspace => {
                    if state.query_input.backspace() {
                        let count = Self::keybinding_candidates(&state).len();
                        Self::keybindings_clamp_selected(&mut state, count);
                    }
                }
                KeyCode::Left => state.query_input.move_left(),
                KeyCode::Right => state.query_input.move_right(),
                KeyCode::Down => {
                    if candidate_count > 0 {
                        state.query_active = false;
                        state.selected = 0;
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.query_input.insert_char(ch);
                    let count = Self::keybinding_candidates(&state).len();
                    Self::keybindings_clamp_selected(&mut state, count);
                }
                _ => {}
            }
            return InputMode::Keybindings { state };
        }

        match key.code {
            KeyCode::Esc => return InputMode::Normal,
            KeyCode::Char('q') if key.modifiers.is_empty() => return InputMode::Normal,
            KeyCode::Char('/')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.query_active = true;
                state.query_input.move_end();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                Self::keybindings_move_down(&mut state, candidate_count);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                Self::keybindings_move_up(&mut state);
            }
            KeyCode::Home | KeyCode::Char('g') => state.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                state.selected = candidate_count.saturating_sub(1);
            }
            _ => {}
        }
        InputMode::Keybindings { state }
    }

    fn keybindings_move_down(state: &mut KeybindingsState, candidate_count: usize) {
        if candidate_count > 0 && state.selected + 1 < candidate_count {
            state.selected += 1;
        }
    }

    fn keybindings_move_up(state: &mut KeybindingsState) {
        state.selected = state.selected.saturating_sub(1);
    }
}
