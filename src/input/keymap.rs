use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::window_manager::{Direction, SplitAxis};

const DEFAULT_PREFIX_KEY: &str = "C-j";

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Copy the active mouse text selection to the clipboard.
    CopySelection,
    /// Copy the spectra version string to the clipboard.
    CopyVersion,
    /// Stage the image on the client's OS clipboard to a server-side temp
    /// file and paste the quoted path into the focused pane.
    PasteImage,
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
    /// Open the keyboard-shortcut cheat sheet overlay.
    ShowKeybindings,
    /// Run an arbitrary shell command bound via a `run:` binding value.
    RunShell(String),
}

fn direction_word(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Down => "down",
        Direction::Up => "up",
        Direction::Right => "right",
    }
}

impl CommandAction {
    /// Human-readable description shown in the keybinding cheat sheet.
    pub fn help_label(&self) -> String {
        match self {
            Self::Split(SplitAxis::Vertical) => "Split pane vertically".to_string(),
            Self::Split(SplitAxis::Horizontal) => "Split pane horizontally".to_string(),
            Self::Focus(direction) => format!("Focus {} pane", direction_word(*direction)),
            Self::FocusNextPane => "Focus next pane".to_string(),
            Self::FocusPrevPane => "Focus previous pane".to_string(),
            Self::ClosePane => "Close pane".to_string(),
            Self::Quit => "Quit".to_string(),
            Self::DetachClient => "Detach client".to_string(),
            Self::SystemTree => "Open tree (sessions/windows/panes)".to_string(),
            Self::SideWindowTree => "Toggle side window tree".to_string(),
            Self::PeekAllWindows => "Peek all panes in session".to_string(),
            Self::NextWindow => "Next window".to_string(),
            Self::PrevWindow => "Previous window".to_string(),
            Self::SelectWindow(number) => format!("Select window {number}"),
            Self::Resize(direction) => format!("Resize pane {}", direction_word(*direction)),
            Self::SwapPrevWindow => "Swap with previous window".to_string(),
            Self::SwapNextWindow => "Swap with next window".to_string(),
            Self::SaveLayout => "Save layout".to_string(),
            Self::WriteLog => "Write log entry".to_string(),
            Self::WriteScrollback => "Write scrollback to file".to_string(),
            Self::OpenPaneBufferInEditor => "Open pane buffer in editor".to_string(),
            Self::RenameSession => "Rename session".to_string(),
            Self::NextSession => "Next session".to_string(),
            Self::PrevSession => "Previous session".to_string(),
            Self::NewSession => "New session".to_string(),
            Self::NewWindow => "New window".to_string(),
            Self::EnterCursorMode => "Enter cursor mode".to_string(),
            Self::LeaveCursorMode => "Leave cursor mode".to_string(),
            Self::CopySelection => "Copy mouse selection".to_string(),
            Self::CopyVersion => "Copy spectra version".to_string(),
            Self::PasteImage => "Paste clipboard image as file path".to_string(),
            Self::CommandPalette => "Open command palette".to_string(),
            Self::ToggleZoom => "Toggle zoom".to_string(),
            Self::ToggleSynchronizePanes => "Toggle synchronize panes".to_string(),
            Self::ReloadConfig => "Reload config".to_string(),
            Self::CreateDefaultConfig => "Create default config".to_string(),
            Self::KillSession => "Kill session".to_string(),
            Self::CloseWindow => "Close window".to_string(),
            Self::OpenConfigInEditor => "Open config in editor".to_string(),
            Self::EnterLockMode => "Enter lock mode".to_string(),
            Self::LeaveLockMode => "Leave lock mode".to_string(),
            Self::ShowKeybindings => "Show keyboard shortcuts".to_string(),
            Self::RunShell(command) => format!("Run shell: {command}"),
        }
    }
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

                if let Some(action) = self.bindings.prefix_bindings.get(key_name).cloned() {
                    if action.should_exit_prefix_mode(self.bindings.prefix_sticky) {
                        self.prefix_active = false;
                    }
                    return InputAction::Command(action);
                }
                // Unbound keys pass through to the pane and exit prefix mode
                self.prefix_active = false;
            }

            if let Some(action) = self.bindings.global_bindings.get(key_name).cloned() {
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
            .and_then(|key_name| self.bindings.global_bindings.get(&key_name).cloned())
    }

    /// Display name of the configured prefix key (e.g. `C-j`).
    pub fn prefix_key_display(&self) -> &str {
        &self.bindings.prefix_key
    }

    /// Whether prefix mode stays active after a bound key runs. When sticky,
    /// a follow-up chord key (e.g. a quit confirmation) needs only the key,
    /// not the prefix again.
    pub fn prefix_sticky(&self) -> bool {
        self.bindings.prefix_sticky
    }

    /// All active bindings as `(key display, description)` pairs for the
    /// keyboard-shortcut cheat sheet. Prefix bindings are prefixed with the
    /// prefix key; global bindings are shown as-is. Sorted by description so
    /// related actions group together.
    pub fn keybinding_help_rows(&self) -> Vec<(String, String)> {
        let prefix = &self.bindings.prefix_key;
        let mut rows: Vec<(String, String)> = self
            .bindings
            .prefix_bindings
            .iter()
            .map(|(key, action)| (format!("{prefix} {key}"), action.help_label()))
            .chain(
                self.bindings
                    .global_bindings
                    .iter()
                    .map(|(key, action)| (key.clone(), action.help_label())),
            )
            .collect();
        rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        rows
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
    map.insert("y".to_string(), CommandAction::CopySelection);
    map.insert("v".to_string(), CommandAction::PasteImage);
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
    map.insert("?".to_string(), CommandAction::ShowKeybindings);

    for digit in 0..=9 {
        let label = digit.to_string();
        let index = if digit == 0 { 10 } else { digit as usize };
        map.insert(label, CommandAction::SelectWindow(index));
    }

    map
}

fn default_global_bindings() -> HashMap<String, CommandAction> {
    let mut map = HashMap::new();
    // Cmd+C copies the mouse selection. Reaches spectra only from terminals
    // that report super-modified keys (kitty keyboard protocol) and forward
    // cmd+c when they have no selection of their own (e.g. ghostty). With no
    // active selection the key falls through to the pane (see
    // `App::handle_key`).
    map.insert("D-c".to_string(), CommandAction::CopySelection);
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
    let trimmed = spec.trim();
    if let Some(command) = trimmed.strip_prefix("run:") {
        let command = command.trim();
        // An empty command is treated like an unknown action name: the
        // override is ignored and the default binding stays in place.
        if command.is_empty() {
            return None;
        }
        return Some(CommandAction::RunShell(command.to_string()));
    }

    let normalized = trimmed.to_ascii_lowercase().replace(['_', ' '], "-");

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
        "copy-selection" | "copy-mouse-selection" => Some(CommandAction::CopySelection),
        "paste-image" | "image-paste" | "paste-clipboard-image" => Some(CommandAction::PasteImage),
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
        "show-keybindings" | "keybindings" | "list-keys" | "show-help" | "help" => {
            Some(CommandAction::ShowKeybindings)
        }

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
    let mut super_key = false;

    // The last token is always the key; everything before it must be a
    // modifier. (Treating modifier-looking tokens as modifiers everywhere
    // would make keys that share a modifier letter — `C-c`, `C-s`, `cmd+c`
    // — impossible to spell.)
    let mut tokens: Vec<&str> = cleaned.split('-').filter(|part| !part.is_empty()).collect();
    let key = tokens.pop()?;
    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "c" | "ctrl" | "control" => ctrl = true,
            "m" | "meta" | "alt" => alt = true,
            "s" | "shift" => shift = true,
            // vim-style `D-c` plus the spelled-out names.
            "d" | "cmd" | "command" | "super" | "win" => super_key = true,
            _ => return None,
        }
    }

    let key = canonical_key_name(key)?;

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
    if super_key {
        modifiers.push("D");
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
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
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
    if key.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("D");
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

    // Super-modified text keys have no legacy encoding; sending the bare
    // character would make cmd+key type a stray letter into the pane.
    // (Panes with the kitty protocol active get these via
    // `encode_key_to_bytes_kitty` instead.)
    if mods.contains(KeyModifiers::SUPER) && matches!(key.code, KeyCode::Char(_)) {
        return None;
    }

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

/// Kitty keyboard protocol flag: disambiguate escape codes (bit 1).
pub(crate) const KITTY_FLAG_DISAMBIGUATE: u8 = 0b1;
/// Kitty keyboard protocol flag: report all keys as escape codes (bit 8).
pub(crate) const KITTY_FLAG_REPORT_ALL: u8 = 0b1000;

/// Encode a key for a pane whose guest enabled the kitty keyboard protocol.
///
/// Lightweight subset following the spec's progressive-enhancement rules:
///
/// - bit 1 (disambiguate escape codes): Esc and keys whose legacy encodings
///   were ambiguous (ctrl+key C0 bytes, alt+key ESC prefixes, modified
///   Enter/Tab/Backspace) are reported as `CSI code;mods u`. Plain text keys
///   keep producing their text, plain Enter/Tab/Backspace keep their C0
///   encodings ("the only exceptions are the Enter, Tab and Backspace keys"),
///   and functional keys keep their legacy `CSI 1;mods{ABCDHF}` /
///   `CSI num;mods~` encodings per the spec.
/// - bit 8 (report all keys as escape codes): additionally, plain text keys
///   and plain Enter/Tab/Backspace are reported as `CSI code;mods u`.
/// - bits 2 (event types), 4 (alternate keys) and 16 (associated text) are
///   tracked by the per-pane state machine but intentionally not encoded in
///   this v1 (release/repeat events are filtered out before reaching the
///   server, and crossterm key events carry no alternate-key payload).
///
/// The modifier field is always emitted explicitly (`;1` for none), which
/// the protocol permits since the field defaults to 1.
pub(crate) fn encode_key_to_bytes_kitty(key: KeyEvent, kitty_flags: u8) -> Option<Vec<u8>> {
    if kitty_flags & (KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_ALL) == 0 {
        return encode_key_to_bytes(key);
    }
    let report_all = kitty_flags & KITTY_FLAG_REPORT_ALL != 0;
    let mods = key.modifiers;
    let m = kitty_modifier(mods);

    match key.code {
        // Under disambiguation, Esc always gets a CSI u encoding so it can
        // never be confused with the start of another escape sequence.
        KeyCode::Esc => Some(kitty_csi_u(27, m)),

        // Enter/Tab/Backspace keep their legacy C0 encodings while
        // unmodified, unless report-all-keys upgrades them to CSI u.
        KeyCode::Enter if m == 1 && !report_all => Some(vec![b'\r']),
        KeyCode::Tab if m == 1 && !report_all => Some(vec![b'\t']),
        KeyCode::Backspace if m == 1 && !report_all => Some(vec![0x7f]),
        KeyCode::Enter => Some(kitty_csi_u(13, m)),
        KeyCode::Tab => Some(kitty_csi_u(9, m)),
        KeyCode::Backspace => Some(kitty_csi_u(127, m)),
        // Shift+Tab is Tab with the shift modifier in kitty terms.
        KeyCode::BackTab => Some(kitty_csi_u(9, kitty_modifier(mods | KeyModifiers::SHIFT))),

        KeyCode::Char(c) => {
            if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
                // Modified text keys had ambiguous legacy encodings, so they
                // are disambiguated as CSI u. The code is the lowercase
                // codepoint per the spec's unicode-key-code rule.
                let code = c.to_lowercase().next().unwrap_or(c);
                Some(kitty_csi_u(code as u32, m))
            } else if report_all {
                Some(kitty_csi_u(c as u32, m))
            } else {
                // Plain (or shift-only) text keys still produce their text.
                Some(c.to_string().into_bytes())
            }
        }

        // Arrows/Home/End keep `CSI 1;mods{ABCDHF}` and Insert/Delete/Page*
        // and F-keys keep `CSI num;mods~` — the legacy encoder already emits
        // these kitty-compatible forms.
        _ => encode_key_to_bytes(key),
    }
}

fn kitty_csi_u(code: u32, modifier: u8) -> Vec<u8> {
    format!("\x1b[{code};{modifier}u").into_bytes()
}

/// Kitty modifier field value: the xterm value plus the kitty-defined
/// super bit.
fn kitty_modifier(mods: KeyModifiers) -> u8 {
    let mut n = xterm_modifier(mods);
    if mods.contains(KeyModifiers::SUPER) {
        n += 8;
    }
    n
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

    use super::{
        CommandAction, InputAction, KITTY_FLAG_DISAMBIGUATE, KITTY_FLAG_REPORT_ALL, KeyMapper,
        encode_key_to_bytes, encode_key_to_bytes_kitty, parse_action,
    };
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
    fn prefix_question_shows_keybindings() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert_eq!(action, InputAction::Command(CommandAction::ShowKeybindings));
    }

    #[test]
    fn show_keybindings_action_names_parse() {
        for name in [
            "show-keybindings",
            "keybindings",
            "list-keys",
            "help",
            "show-help",
        ] {
            assert_eq!(
                parse_action(name),
                Some(CommandAction::ShowKeybindings),
                "`{name}` should parse to ShowKeybindings"
            );
        }
    }

    #[test]
    fn keybinding_help_rows_include_prefixed_key_and_label() {
        let mapper = KeyMapper::new();
        let rows = mapper.keybinding_help_rows();
        let prefix = mapper.prefix_key_display();
        assert!(
            rows.iter().any(|(keys, description)| {
                keys == &format!("{prefix} ?") && description == "Show keyboard shortcuts"
            }),
            "cheat sheet should list the prefixed `?` binding with its label"
        );
        // Rows are sorted by description so related actions group together.
        let descriptions: Vec<&String> = rows.iter().map(|(_, d)| d).collect();
        let mut sorted = descriptions.clone();
        sorted.sort();
        assert_eq!(descriptions, sorted, "rows must be sorted by description");
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
    fn super_c_maps_to_copy_selection_globally() {
        let mut mapper = KeyMapper::new();
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Command(CommandAction::CopySelection));
        // Terminals may report cmd+shift-adjacent case; the canonical name
        // lowercases super-modified chars, so 'C' matches too.
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Command(CommandAction::CopySelection));
    }

    #[test]
    fn unbound_super_char_is_ignored_not_typed() {
        let mut mapper = KeyMapper::new();
        // Super-modified chars have no legacy encoding; sending the bare
        // char would type a stray letter into the pane.
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Ignore);
    }

    #[test]
    fn cmd_binding_specs_normalize_to_super_modifier() {
        let mut globals = HashMap::new();
        globals.insert("cmd+y".to_string(), "copy-selection".to_string());
        globals.insert("super+u".to_string(), "toggle-zoom".to_string());
        globals.insert("D-i".to_string(), "next-window".to_string());
        let mut mapper = KeyMapper::with_config(None, true, &HashMap::new(), &globals);

        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Command(CommandAction::CopySelection));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Command(CommandAction::ToggleZoom));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Command(CommandAction::NextWindow));
    }

    #[test]
    fn cmd_c_default_binding_can_be_disabled() {
        let mut globals = HashMap::new();
        globals.insert("cmd+c".to_string(), "none".to_string());
        let mut mapper = KeyMapper::with_config(None, true, &HashMap::new(), &globals);
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
        assert_eq!(action, InputAction::Ignore);
    }

    #[test]
    fn lowercase_d_stays_a_key_not_a_super_modifier() {
        let mut prefix = HashMap::new();
        prefix.insert("C-d".to_string(), "toggle-zoom".to_string());
        let mut mapper = KeyMapper::with_config(None, true, &prefix, &HashMap::new());
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Command(CommandAction::ToggleZoom));
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
    fn parses_paste_image_action_names() {
        assert_eq!(parse_action("paste-image"), Some(CommandAction::PasteImage));
        assert_eq!(parse_action("image-paste"), Some(CommandAction::PasteImage));
        assert_eq!(
            parse_action("paste_clipboard_image"),
            Some(CommandAction::PasteImage)
        );
    }

    #[test]
    fn prefix_v_triggers_paste_image_by_default() {
        let mut mapper = KeyMapper::new();
        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::PasteImage));
    }

    #[test]
    fn parses_run_binding_into_run_shell_action() {
        assert_eq!(
            parse_action("run: echo hi"),
            Some(CommandAction::RunShell("echo hi".to_string()))
        );
        assert_eq!(
            parse_action("  run:notify-send 'hello'  "),
            Some(CommandAction::RunShell("notify-send 'hello'".to_string()))
        );
    }

    #[test]
    fn rejects_empty_run_binding() {
        assert_eq!(parse_action("run:"), None);
        assert_eq!(parse_action("run:   "), None);
    }

    #[test]
    fn run_binding_works_in_prefix_and_global_maps() {
        let mut prefix = HashMap::new();
        prefix.insert("x".to_string(), "run: touch /tmp/marker".to_string());
        let mut global = HashMap::new();
        global.insert("M-x".to_string(), "run: echo global".to_string());

        let mut mapper = KeyMapper::with_config(None, true, &prefix, &global);

        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::RunShell("touch /tmp/marker".to_string()))
        );

        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(
            action,
            InputAction::Command(CommandAction::RunShell("echo global".to_string()))
        );
    }

    #[test]
    fn unbind_still_removes_default_binding() {
        let mut prefix = HashMap::new();
        prefix.insert("x".to_string(), "none".to_string());

        let mut mapper = KeyMapper::with_config(None, true, &prefix, &HashMap::new());

        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        // Unbound prefix key passes through to the pane instead of closing it.
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::SendBytes(vec![b'x']));
    }

    #[test]
    fn empty_run_binding_keeps_default_binding() {
        let mut prefix = HashMap::new();
        prefix.insert("x".to_string(), "run:  ".to_string());

        let mut mapper = KeyMapper::with_config(None, true, &prefix, &HashMap::new());

        mapper.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let action = mapper.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command(CommandAction::ClosePane));
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

    #[test]
    fn kitty_esc_is_csi_u_under_disambiguate() {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_bytes_kitty(key, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[27;1u".to_vec())
        );
    }

    #[test]
    fn kitty_ctrl_letter_is_csi_u_under_disambiguate() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key_to_bytes_kitty(key, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[99;5u".to_vec())
        );
    }

    #[test]
    fn kitty_alt_letter_is_csi_u_under_disambiguate() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(
            encode_key_to_bytes_kitty(key, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[120;3u".to_vec())
        );
    }

    #[test]
    fn kitty_super_letter_is_csi_u_under_disambiguate() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
        assert_eq!(
            encode_key_to_bytes_kitty(key, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[99;9u".to_vec())
        );
    }

    #[test]
    fn kitty_plain_text_stays_text_under_disambiguate() {
        let plain = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_bytes_kitty(plain, KITTY_FLAG_DISAMBIGUATE),
            Some(b"a".to_vec())
        );
        // Shift-only text keys keep producing their (shifted) text.
        let shifted = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(
            encode_key_to_bytes_kitty(shifted, KITTY_FLAG_DISAMBIGUATE),
            Some(b"A".to_vec())
        );
    }

    #[test]
    fn kitty_plain_text_is_csi_u_under_report_all() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_bytes_kitty(key, KITTY_FLAG_REPORT_ALL),
            Some(b"\x1b[97;1u".to_vec())
        );
    }

    #[test]
    fn kitty_enter_tab_backspace_keep_c0_unless_modified() {
        let flags = KITTY_FLAG_DISAMBIGUATE;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(encode_key_to_bytes_kitty(enter, flags), Some(vec![b'\r']));
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(encode_key_to_bytes_kitty(tab, flags), Some(vec![b'\t']));
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_bytes_kitty(backspace, flags),
            Some(vec![0x7f])
        );

        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(
            encode_key_to_bytes_kitty(ctrl_enter, flags),
            Some(b"\x1b[13;5u".to_vec())
        );
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key_to_bytes_kitty(shift_tab, flags),
            Some(b"\x1b[9;2u".to_vec())
        );
        let alt_backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(
            encode_key_to_bytes_kitty(alt_backspace, flags),
            Some(b"\x1b[127;3u".to_vec())
        );

        // Report-all upgrades even the unmodified C0 keys to CSI u.
        assert_eq!(
            encode_key_to_bytes_kitty(enter, KITTY_FLAG_REPORT_ALL),
            Some(b"\x1b[13;1u".to_vec())
        );
    }

    #[test]
    fn kitty_arrows_keep_legacy_csi_forms() {
        let flags = KITTY_FLAG_DISAMBIGUATE;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_bytes_kitty(up, flags),
            Some(b"\x1b[A".to_vec())
        );
        let ctrl_up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(
            encode_key_to_bytes_kitty(ctrl_up, flags),
            Some(b"\x1b[1;5A".to_vec())
        );
        let shift_alt_right =
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT | KeyModifiers::ALT);
        assert_eq!(
            encode_key_to_bytes_kitty(shift_alt_right, flags),
            Some(b"\x1b[1;4C".to_vec())
        );
    }

    #[test]
    fn kitty_zero_flags_falls_back_to_legacy_encoding() {
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            assert_eq!(encode_key_to_bytes_kitty(key, 0), encode_key_to_bytes(key));
        }
    }
}
