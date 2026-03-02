use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::window_manager::{Direction, SplitAxis};

const DEFAULT_PREFIX_KEY: &str = "C-j";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    Split(SplitAxis),
    Focus(Direction),
    FocusNextPane,
    FocusPrevPane,
    ClosePane,
    Quit,
    DetachClient,
    SystemTree,
    SideWindowTree,
    PeekAllWindows,
    NextWindow,
    PrevWindow,
    SelectWindow(usize),
    Resize(Direction),
    SwapPrevWindow,
    SwapNextWindow,
    SaveLayout,
    WriteLog,
    WriteScrollback,
    OpenPaneBufferInEditor,
    RenameSession,
    NextSession,
    PrevSession,
    NewSession,
    NewWindow,
    EnterCursorMode,
    LeaveCursorMode,
    CommandPalette,
    ToggleZoom,
    ToggleSynchronizePanes,
    ReloadConfig,
    CreateDefaultConfig,
    KillSession,
    CloseWindow,
    OpenConfigInEditor,
    EnterLockMode,
    LeaveLockMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Command(CommandAction),
    SendBytes(Vec<u8>),
    Ignore,
}

#[derive(Debug)]
struct KeyBindings {
    prefix_key: String,
    prefix_sticky: bool,
    prefix_bindings: HashMap<String, CommandAction>,
    global_bindings: HashMap<String, CommandAction>,
}

#[derive(Debug, Clone)]
pub struct KeyMapper {
    prefix_active: bool,
    bindings: Arc<KeyBindings>,
}

impl Default for KeyMapper {
    fn default() -> Self {
        Self::with_config(None, true, &HashMap::new(), &HashMap::new())
    }
}

impl KeyMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(
        prefix_key: Option<&str>,
        prefix_sticky: bool,
        prefix_overrides: &HashMap<String, String>,
        global_overrides: &HashMap<String, String>,
    ) -> Self {
        let prefix_key = prefix_key
            .and_then(normalize_binding_key)
            .unwrap_or_else(|| DEFAULT_PREFIX_KEY.to_string());

        let mut prefix_bindings = default_prefix_bindings();
        apply_overrides(&mut prefix_bindings, prefix_overrides);

        let mut global_bindings = default_global_bindings();
        apply_overrides(&mut global_bindings, global_overrides);

        Self {
            prefix_active: false,
            bindings: Arc::new(KeyBindings {
                prefix_key,
                prefix_sticky,
                prefix_bindings,
                global_bindings,
            }),
        }
    }

    pub fn prefix_active(&self) -> bool {
        self.prefix_active
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        let key_name = canonical_key_event(&key);

        if let Some(key_name) = &key_name {
            if key_name == &self.bindings.prefix_key {
                if self.prefix_active {
                    // Double-press: pass the key through to the pane and exit prefix mode
                    self.prefix_active = false;
                    return match encode_key_to_bytes(key) {
                        Some(bytes) => InputAction::SendBytes(bytes),
                        None => InputAction::Ignore,
                    };
                } else {
                    self.prefix_active = true;
                    return InputAction::Ignore;
                }
            }

            if self.prefix_active {
                if key.code == KeyCode::Esc {
                    self.prefix_active = false;
                    return InputAction::Ignore;
                }

                if let Some(action) = self.bindings.prefix_bindings.get(key_name).copied() {
                    if action.should_exit_prefix_mode(self.bindings.prefix_sticky) {
                        self.prefix_active = false;
                    }
                    return InputAction::Command(action);
                }
                // Unbound keys pass through to the pane and exit prefix mode
                self.prefix_active = false;
            }

            if let Some(action) = self.bindings.global_bindings.get(key_name).copied() {
                return InputAction::Command(action);
            }
        }

        match encode_key_to_bytes(key) {
            Some(bytes) => InputAction::SendBytes(bytes),
            None => InputAction::Ignore,
        }
    }

    pub fn check_global_action(&self, key: KeyEvent) -> Option<CommandAction> {
        canonical_key_event(&key)
            .and_then(|key_name| self.bindings.global_bindings.get(&key_name).copied())
    }
}

impl CommandAction {
    fn should_exit_prefix_mode(&self, prefix_sticky: bool) -> bool {
        match self {
            Self::SystemTree | Self::SideWindowTree => true,
            _ => !prefix_sticky,
        }
    }
}

fn default_prefix_bindings() -> HashMap<String, CommandAction> {
    let mut map = HashMap::new();

    map.insert("|".to_string(), CommandAction::Split(SplitAxis::Vertical));
    map.insert(
        "\"".to_string(),
        CommandAction::Split(SplitAxis::Horizontal),
    );
    map.insert("Left".to_string(), CommandAction::Focus(Direction::Left));
    map.insert("Down".to_string(), CommandAction::Focus(Direction::Down));
    map.insert("Up".to_string(), CommandAction::Focus(Direction::Up));
    map.insert("Right".to_string(), CommandAction::Focus(Direction::Right));
    map.insert("x".to_string(), CommandAction::ClosePane);
    map.insert("q".to_string(), CommandAction::Quit);
    map.insert("d".to_string(), CommandAction::DetachClient);

    map.insert("w".to_string(), CommandAction::SystemTree);
    map.insert("e".to_string(), CommandAction::SideWindowTree);
    map.insert("Tab".to_string(), CommandAction::SystemTree);
    map.insert("W".to_string(), CommandAction::PeekAllWindows);
    map.insert("n".to_string(), CommandAction::NewSession);
    map.insert("c".to_string(), CommandAction::NewWindow);
    map.insert("[".to_string(), CommandAction::EnterCursorMode);
    map.insert("p".to_string(), CommandAction::CommandPalette);
    map.insert("z".to_string(), CommandAction::ToggleZoom);
    map.insert("S".to_string(), CommandAction::ToggleSynchronizePanes);
    map.insert("r".to_string(), CommandAction::ReloadConfig);
    map.insert("O".to_string(), CommandAction::FocusPrevPane);
    map.insert("o".to_string(), CommandAction::FocusNextPane);

    map.insert("{".to_string(), CommandAction::SwapPrevWindow);
    map.insert("}".to_string(), CommandAction::SwapNextWindow);
    map.insert("C-Left".to_string(), CommandAction::Resize(Direction::Left));
    map.insert("C-Down".to_string(), CommandAction::Resize(Direction::Down));
    map.insert("C-Up".to_string(), CommandAction::Resize(Direction::Up));
    map.insert(
        "C-Right".to_string(),
        CommandAction::Resize(Direction::Right),
    );

    map.insert("$".to_string(), CommandAction::RenameSession);
    map.insert("C-s".to_string(), CommandAction::SaveLayout);
    map.insert("l".to_string(), CommandAction::WriteLog);
    map.insert("P".to_string(), CommandAction::WriteScrollback);

    map.insert("(".to_string(), CommandAction::PrevSession);
    map.insert(")".to_string(), CommandAction::NextSession);
    map.insert("s".to_string(), CommandAction::SystemTree);

    for digit in 0..=9 {
        let label = digit.to_string();
        let index = if digit == 0 { 10 } else { digit as usize };
        map.insert(label, CommandAction::SelectWindow(index));
    }

    map
}

fn default_global_bindings() -> HashMap<String, CommandAction> {
    let mut map = HashMap::new();
    map.insert("M-Left".to_string(), CommandAction::Focus(Direction::Left));
    map.insert("M-Down".to_string(), CommandAction::Focus(Direction::Down));
    map.insert("M-Up".to_string(), CommandAction::Focus(Direction::Up));
    map.insert(
        "M-Right".to_string(),
        CommandAction::Focus(Direction::Right),
    );
    map
}

fn apply_overrides(map: &mut HashMap<String, CommandAction>, overrides: &HashMap<String, String>) {
    for (key, value) in overrides {
        let Some(key) = normalize_binding_key(key) else {
            continue;
        };
        let normalized = value.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "none" | "unbound" | "disabled") {
            map.remove(&key);
            continue;
        }
        if let Some(action) = parse_action(value) {
            map.insert(key, action);
        }
    }
}

fn parse_action(spec: &str) -> Option<CommandAction> {
    let normalized = spec.trim().to_ascii_lowercase().replace(['_', ' '], "-");

    match normalized.as_str() {
        "split-vertical" => Some(CommandAction::Split(SplitAxis::Vertical)),
        "split-horizontal" => Some(CommandAction::Split(SplitAxis::Horizontal)),

        "focus-left" => Some(CommandAction::Focus(Direction::Left)),
        "focus-down" => Some(CommandAction::Focus(Direction::Down)),
        "focus-up" => Some(CommandAction::Focus(Direction::Up)),
        "focus-right" => Some(CommandAction::Focus(Direction::Right)),
        "focus-next-pane" | "next-pane" => Some(CommandAction::FocusNextPane),
        "focus-prev-pane" | "focus-previous-pane" | "prev-pane" => {
            Some(CommandAction::FocusPrevPane)
        }

        "close-pane" => Some(CommandAction::ClosePane),
        "quit" => Some(CommandAction::Quit),
        "detach-client" | "detach" => Some(CommandAction::DetachClient),

        "window-list" | "window-tree" | "session-list" => Some(CommandAction::SystemTree),
        "side-window-tree" | "toggle-side-window-tree" => Some(CommandAction::SideWindowTree),
        "peek-all-windows" | "peek-all-panes" | "peek-session-panes" => {
            Some(CommandAction::PeekAllWindows)
        }
        "next-window" => Some(CommandAction::NextWindow),
        "prev-window" => Some(CommandAction::PrevWindow),

        "resize-left" => Some(CommandAction::Resize(Direction::Left)),
        "resize-down" => Some(CommandAction::Resize(Direction::Down)),
        "resize-up" => Some(CommandAction::Resize(Direction::Up)),
        "resize-right" => Some(CommandAction::Resize(Direction::Right)),

        "swap-prev-window" => Some(CommandAction::SwapPrevWindow),
        "swap-next-window" => Some(CommandAction::SwapNextWindow),

        "save-layout" => Some(CommandAction::SaveLayout),
        "write-log" => Some(CommandAction::WriteLog),
        "write-scrollback" => Some(CommandAction::WriteScrollback),
        "open-current-pane-buffer-in-editor"
        | "open-current-pane-buffef-in-editor"
        | "open-current-pane's-buffer-in-editor"
        | "open-current-pane's-buffef-in-editor"
        | "open-current-panes-buffer-in-editor"
        | "open-current-panes-buffef-in-editor" => Some(CommandAction::OpenPaneBufferInEditor),
        "rename-session" => Some(CommandAction::RenameSession),

        "next-session" => Some(CommandAction::NextSession),
        "prev-session" => Some(CommandAction::PrevSession),
        "new-session" => Some(CommandAction::NewSession),
        "new-window" => Some(CommandAction::NewWindow),
        "copy-mode" | "enter-cursor-mode" | "cursor-mode" => Some(CommandAction::EnterCursorMode),
        "leave-cursor-mode" | "exit-cursor-mode" => Some(CommandAction::LeaveCursorMode),
        "command-palette" | "open-command-palette" => Some(CommandAction::CommandPalette),
        "toggle-zoom" | "zoom-toggle" => Some(CommandAction::ToggleZoom),
        "synchronize-panes" | "toggle-synchronize-panes" => {
            Some(CommandAction::ToggleSynchronizePanes)
        }
        "reload-config" | "source-file" => Some(CommandAction::ReloadConfig),
        "create-default-config" | "config-create-default" => {
            Some(CommandAction::CreateDefaultConfig)
        }
        "kill-session" | "kill-current-session" => Some(CommandAction::KillSession),
        "close-window" | "close-current-window" => Some(CommandAction::CloseWindow),
        "open-config-in-editor" | "edit-config" => Some(CommandAction::OpenConfigInEditor),
        "enter-lock-mode" | "lock" => Some(CommandAction::EnterLockMode),
        "leave-lock-mode" | "unlock" => Some(CommandAction::LeaveLockMode),

        _ => parse_select_window(&normalized),
    }
}

fn parse_select_window(spec: &str) -> Option<CommandAction> {
    let number = spec.strip_prefix("select-window-")?.parse::<usize>().ok()?;
    if number == 0 {
        return None;
    }
    Some(CommandAction::SelectWindow(number))
}

fn normalize_binding_key(spec: &str) -> Option<String> {
    let cleaned = spec.trim();
    if cleaned.is_empty() {
        return None;
    }

    let cleaned = cleaned.replace('+', "-");
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None;

    for token in cleaned.split('-').filter(|part| !part.is_empty()) {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "c" | "ctrl" | "control" => ctrl = true,
            "m" | "meta" | "alt" => alt = true,
            "s" | "shift" => shift = true,
            _ => key = Some(token.to_string()),
        }
    }

    let key = key?;
    let key = canonical_key_name(&key)?;

    let mut modifiers = Vec::new();
    if ctrl {
        modifiers.push("C");
    }
    if alt {
        modifiers.push("M");
    }
    if shift && key.chars().count() > 1 {
        modifiers.push("S");
    }

    if modifiers.is_empty() {
        Some(key)
    } else {
        Some(format!("{}-{key}", modifiers.join("-")))
    }
}

fn canonical_key_event(key: &KeyEvent) -> Option<String> {
    let key_name = match key.code {
        KeyCode::Char(c) => {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                c.to_ascii_lowercase().to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "S-Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    };

    let is_char = matches!(key.code, KeyCode::Char(_));
    let is_backtab = matches!(key.code, KeyCode::BackTab);
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("C");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("M");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) && !is_char && !is_backtab {
        modifiers.push("S");
    }

    if modifiers.is_empty() {
        Some(key_name)
    } else {
        Some(format!("{}-{key_name}", modifiers.join("-")))
    }
}

fn canonical_key_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some("Enter".to_string()),
        "tab" => Some("Tab".to_string()),
        "backtab" => Some("S-Tab".to_string()),
        "backspace" => Some("Backspace".to_string()),
        "esc" | "escape" => Some("Esc".to_string()),
        "left" => Some("Left".to_string()),
        "right" => Some("Right".to_string()),
        "up" => Some("Up".to_string()),
        "down" => Some("Down".to_string()),
        "home" => Some("Home".to_string()),
        "end" => Some("End".to_string()),
        "delete" | "del" => Some("Delete".to_string()),
        "pageup" | "pgup" => Some("PageUp".to_string()),
        "pagedown" | "pgdown" => Some("PageDown".to_string()),
        _ if name.chars().count() == 1 => Some(name.to_string()),
        _ => None,
    }
}

pub(crate) fn encode_key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let mods = key.modifiers;

    // Ctrl+letter → control character byte, with optional Alt (ESC) prefix
    if mods.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
    {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            let ctrl_byte = (lower as u8) - b'a' + 1;
            if mods.contains(KeyModifiers::ALT) {
                return Some(vec![0x1b, ctrl_byte]);
            }
            return Some(vec![ctrl_byte]);
        }
    }

    // Alt+char → ESC prefix + char
    if mods.contains(KeyModifiers::ALT)
        && !mods.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
    {
        let mut bytes = vec![0x1b];
        let mut buf = [0u8; 4];
        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return Some(bytes);
    }

    // CSI letter keys: arrows, Home, End
    // Plain: \x1b[X   Modified: \x1b[1;{mod}X
    let csi_letter = match key.code {
        KeyCode::Up => Some(b'A'),
        KeyCode::Down => Some(b'B'),
        KeyCode::Right => Some(b'C'),
        KeyCode::Left => Some(b'D'),
        KeyCode::Home => Some(b'H'),
        KeyCode::End => Some(b'F'),
        _ => None,
    };
    if let Some(letter) = csi_letter {
        let m = xterm_modifier(mods);
        if m == 1 {
            return Some(vec![0x1b, b'[', letter]);
        }
        return Some(format!("\x1b[1;{m}{}", letter as char).into_bytes());
    }

    // CSI number~ keys: Delete, PageUp, PageDown, Insert
    // Plain: \x1b[N~   Modified: \x1b[N;{mod}~
    let csi_number = match key.code {
        KeyCode::Insert => Some(2),
        KeyCode::Delete => Some(3),
        KeyCode::PageUp => Some(5),
        KeyCode::PageDown => Some(6),
        _ => None,
    };
    if let Some(num) = csi_number {
        let m = xterm_modifier(mods);
        if m == 1 {
            return Some(format!("\x1b[{num}~").into_bytes());
        }
        return Some(format!("\x1b[{num};{m}~").into_bytes());
    }

    // F-keys: \x1b[N~ with xterm numbering
    if let KeyCode::F(n) = key.code {
        let fkey_num = match n {
            1 => 11,
            2 => 12,
            3 => 13,
            4 => 14,
            5 => 15,
            6 => 17,
            7 => 18,
            8 => 19,
            9 => 20,
            10 => 21,
            11 => 23,
            12 => 24,
            _ => return None,
        };
        let m = xterm_modifier(mods);
        if m == 1 {
            return Some(format!("\x1b[{fkey_num}~").into_bytes());
        }
        return Some(format!("\x1b[{fkey_num};{m}~").into_bytes());
    }

    match key.code {
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),

        // Plain variants
        KeyCode::Enter if mods.is_empty() => Some(vec![b'\r']),
        KeyCode::Tab if mods.is_empty() => Some(vec![b'\t']),
        KeyCode::Backspace if mods.is_empty() => Some(vec![0x7f]),

        // Modified Enter/Tab/Backspace: fixterms format \x1b[27;{mod};{code}~
        KeyCode::Enter => {
            let m = xterm_modifier(mods);
            Some(format!("\x1b[27;{m};13~").into_bytes())
        }
        KeyCode::Tab => {
            let m = xterm_modifier(mods);
            Some(format!("\x1b[27;{m};9~").into_bytes())
        }
        KeyCode::Backspace => {
            let m = xterm_modifier(mods);
            Some(format!("\x1b[27;{m};127~").into_bytes())
        }

        _ => None,
    }
}

/// Compute the xterm modifier parameter value (1 = no modifiers).
fn xterm_modifier(mods: KeyModifiers) -> u8 {
    let mut n = 1u8;
    if mods.contains(KeyModifiers::SHIFT) {
        n += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        n += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        n += 4;
    }
    n
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{CommandAction, InputAction, KeyMapper, parse_action};
    use crate::ui::window_manager::{Direction, SplitAxis};

    #[test]
    fn ctrl_j_enters_prefix_mode() {
        let mut mapper = KeyMapper::new();
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Ignore);
        assert!(mapper.prefix_active());
    }

    #[test]
    fn prefix_split_vertical() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::Split(SplitAxis::Vertical))
        );
        assert!(mapper.prefix_active());
    }

    #[test]
    fn prefix_n_creates_new_session() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::NewSession));
    }

    #[test]
    fn prefix_c_creates_new_window() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::NewWindow));
    }

    #[test]
    fn prefix_bracket_enters_cursor_mode() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::EnterCursorMode));
    }

    #[test]
    fn prefix_p_opens_command_palette() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::CommandPalette));
    }

    #[test]
    fn prefix_e_toggles_side_window_tree() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::SideWindowTree));
    }

    #[test]
    fn prefix_upper_w_peeks_all_windows() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::PeekAllWindows));
    }

    #[test]
    fn prefix_upper_o_selects_prev_pane_history() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::FocusPrevPane));
    }

    #[test]
    fn prefix_o_selects_next_pane_history() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::FocusNextPane));
    }

    #[test]
    fn prefix_z_toggles_zoom() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::ToggleZoom));
    }

    #[test]
    fn prefix_upper_s_toggles_synchronize_panes() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::ToggleSynchronizePanes)
        );
    }

    #[test]
    fn prefix_r_reloads_config() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::ReloadConfig));
    }

    #[test]
    fn prefix_d_detaches_client() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::DetachClient));
    }

    #[test]
    fn prefix_resize_left() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::Resize(Direction::Left))
        );
    }

    #[test]
    fn prefix_number_selects_window() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::SelectWindow(2)));
    }

    #[test]
    fn plain_char_passes_to_pane() {
        let mut mapper = KeyMapper::new();
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::SendBytes(vec![b'a']));
    }

    #[test]
    fn allows_config_overrides() {
        let mut prefix = HashMap::new();
        prefix.insert("v".to_string(), "split-vertical".to_string());

        let mut mapper = KeyMapper::with_config(Some("C-a"), true, &prefix, &HashMap::new());

        mapper.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));

        assert_eq!(
            action,
            InputAction::Command(CommandAction::Split(SplitAxis::Vertical))
        );
    }

    #[test]
    fn parses_legacy_tree_aliases() {
        assert_eq!(parse_action("window-list"), Some(CommandAction::SystemTree));
        assert_eq!(
            parse_action("session-list"),
            Some(CommandAction::SystemTree)
        );
        assert_eq!(parse_action("window-tree"), Some(CommandAction::SystemTree));
        assert_eq!(
            parse_action("side-window-tree"),
            Some(CommandAction::SideWindowTree)
        );
        assert_eq!(
            parse_action("toggle-side-window-tree"),
            Some(CommandAction::SideWindowTree)
        );
        assert_eq!(
            parse_action("peek-all-windows"),
            Some(CommandAction::PeekAllWindows)
        );
        assert_eq!(
            parse_action("peek-all-panes"),
            Some(CommandAction::PeekAllWindows)
        );
        assert_eq!(
            parse_action("detach-client"),
            Some(CommandAction::DetachClient)
        );
        assert_eq!(parse_action("detach"), Some(CommandAction::DetachClient));
        assert_eq!(parse_action("toggle-zoom"), Some(CommandAction::ToggleZoom));
        assert_eq!(
            parse_action("synchronize-panes"),
            Some(CommandAction::ToggleSynchronizePanes)
        );
        assert_eq!(
            parse_action("reload-config"),
            Some(CommandAction::ReloadConfig)
        );
        assert_eq!(
            parse_action("source-file"),
            Some(CommandAction::ReloadConfig)
        );
        assert_eq!(
            parse_action("create-default-config"),
            Some(CommandAction::CreateDefaultConfig)
        );
        assert_eq!(
            parse_action("command-palette"),
            Some(CommandAction::CommandPalette)
        );
        assert_eq!(
            parse_action("open-command-palette"),
            Some(CommandAction::CommandPalette)
        );
        assert_eq!(
            parse_action("copy-mode"),
            Some(CommandAction::EnterCursorMode)
        );
        assert_eq!(
            parse_action("enter-cursor-mode"),
            Some(CommandAction::EnterCursorMode)
        );
        assert_eq!(
            parse_action("leave-cursor-mode"),
            Some(CommandAction::LeaveCursorMode)
        );
        assert_eq!(
            parse_action("open-current-pane-buffer-in-editor"),
            Some(CommandAction::OpenPaneBufferInEditor)
        );
        assert_eq!(
            parse_action("open-current-pane-buffef-in-editor"),
            Some(CommandAction::OpenPaneBufferInEditor)
        );
        assert_eq!(
            parse_action("open current pane's buffef in editor"),
            Some(CommandAction::OpenPaneBufferInEditor)
        );
        assert_eq!(
            parse_action("focus-next-pane"),
            Some(CommandAction::FocusNextPane)
        );
        assert_eq!(
            parse_action("next-pane"),
            Some(CommandAction::FocusNextPane)
        );
        assert_eq!(
            parse_action("focus-previous-pane"),
            Some(CommandAction::FocusPrevPane)
        );
        assert_eq!(
            parse_action("prev-pane"),
            Some(CommandAction::FocusPrevPane)
        );
    }

    #[test]
    fn prefix_key_toggles_off_when_already_active() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(mapper.prefix_active());
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::SendBytes(vec![0x0A]));
        assert!(!mapper.prefix_active());
    }

    #[test]
    fn esc_exits_prefix_mode() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(mapper.prefix_active());
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Ignore);
        assert!(!mapper.prefix_active());
    }

    #[test]
    fn prefix_stays_active_after_command() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::Split(SplitAxis::Vertical))
        );
        assert!(mapper.prefix_active());
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::ClosePane));
        assert!(mapper.prefix_active());
    }

    #[test]
    fn prefix_tree_deactivates_prefix_mode() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::SystemTree));
        assert!(!mapper.prefix_active());
    }

    #[test]
    fn prefix_side_window_tree_deactivates_prefix_mode() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::SideWindowTree));
        assert!(!mapper.prefix_active());
    }

    #[test]
    fn prefix_not_sticky_deactivates_after_command() {
        let mut mapper = KeyMapper::with_config(None, false, &HashMap::new(), &HashMap::new());
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(mapper.prefix_active());
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::Split(SplitAxis::Vertical))
        );
        assert!(!mapper.prefix_active());
    }

    #[test]
    fn prefix_unbound_key_passes_through_to_pane() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(mapper.prefix_active());
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::SendBytes(vec![b'a']));
        assert!(!mapper.prefix_active());
    }
}
