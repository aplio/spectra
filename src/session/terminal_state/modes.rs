use super::*;

/// Mouse reporting level requested by the guest program via DECSET.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseProtocol {
    #[default]
    None,
    /// DECSET 9: button presses only, no modifiers.
    X10,
    /// DECSET 1000: presses, releases and scroll.
    Normal,
    /// DECSET 1002: normal plus drag motion while a button is held.
    ButtonEvent,
    /// DECSET 1003: all motion events.
    AnyEvent,
}

/// Per-screen kitty keyboard protocol state (`CSI > u` push, `CSI < u` pop,
/// `CSI = u` set, `CSI ? u` query).
///
/// This is a lightweight subset: all five defined flag bits are tracked
/// verbatim, but the key-encoding side only acts on bit 1 (disambiguate
/// escape codes) and bit 8 (report all keys as escape codes). Bits 2/4/16
/// (event types / alternate keys / associated text) are stored so queries
/// round-trip but are intentionally not honoured when encoding keys.
#[derive(Debug, Default, Clone)]
pub(super) struct KittyKeyboardStack {
    /// Entries pushed with `CSI > flags u`, newest last.
    stack: Vec<u8>,
    /// Flags in effect when the stack is empty; only `CSI = flags ; mode u`
    /// can make this non-zero.
    base: u8,
}

/// All flag bits defined by the kitty keyboard protocol (1|2|4|8|16).
pub(super) const KITTY_KBD_ALL_FLAGS: u16 = 0b1_1111;

/// Cap on pushed entries so a hostile guest cannot grow the stack without
/// bound. The kitty spec instructs terminals to evict the oldest entry when
/// the stack is full.
pub(super) const KITTY_KBD_STACK_CAP: usize = 16;

impl KittyKeyboardStack {
    pub(super) fn current(&self) -> u8 {
        self.stack.last().copied().unwrap_or(self.base)
    }

    pub(super) fn push(&mut self, flags: u8) {
        if self.stack.len() >= KITTY_KBD_STACK_CAP {
            self.stack.remove(0);
        }
        self.stack.push(flags);
    }

    pub(super) fn pop(&mut self, count: usize) {
        for _ in 0..count {
            if self.stack.pop().is_none() {
                // Popping below the bottom of the stack resets to defaults.
                self.base = 0;
                break;
            }
        }
    }

    pub(super) fn set(&mut self, flags: u8, mode: usize) {
        let new = match mode {
            2 => self.current() | flags,  // mode 2: set the given bits
            3 => self.current() & !flags, // mode 3: clear the given bits
            _ => flags,                   // mode 1 (default): assign all bits
        };
        match self.stack.last_mut() {
            Some(top) => *top = new,
            None => self.base = new,
        }
    }
}

impl TerminalState {
    /// Encode a mouse event for the guest according to its requested mouse
    /// protocol and encoding. `col`/`row` are 0-based pane-local cell
    /// coordinates. Returns `None` when the guest did not ask for this kind
    /// of event (or any mouse reporting at all).
    pub fn encode_mouse_event(
        &self,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: usize,
        row: usize,
    ) -> Option<Vec<u8>> {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

        let protocol = self.grid.mouse_protocol;
        if protocol == MouseProtocol::None {
            return None;
        }

        let button_code = |button: MouseButton| match button {
            MouseButton::Left => 0u16,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };

        let (mut code, is_release) = match kind {
            MouseEventKind::Down(button) => (button_code(button), false),
            MouseEventKind::Up(button) => (button_code(button), true),
            MouseEventKind::Drag(button) => (button_code(button) + 32, false),
            MouseEventKind::Moved => (32 + 3, false),
            MouseEventKind::ScrollUp => (64, false),
            MouseEventKind::ScrollDown => (65, false),
            MouseEventKind::ScrollLeft => (66, false),
            MouseEventKind::ScrollRight => (67, false),
        };

        let wanted = match protocol {
            MouseProtocol::None => false,
            // X10 reports button presses only, without modifiers.
            MouseProtocol::X10 => matches!(kind, MouseEventKind::Down(_)),
            MouseProtocol::Normal => {
                !matches!(kind, MouseEventKind::Drag(_) | MouseEventKind::Moved)
            }
            MouseProtocol::ButtonEvent => !matches!(kind, MouseEventKind::Moved),
            MouseProtocol::AnyEvent => true,
        };
        if !wanted {
            return None;
        }

        if protocol != MouseProtocol::X10 {
            if modifiers.contains(KeyModifiers::SHIFT) {
                code += 4;
            }
            if modifiers.contains(KeyModifiers::ALT) {
                code += 8;
            }
            if modifiers.contains(KeyModifiers::CONTROL) {
                code += 16;
            }
        }

        if self.grid.mouse_sgr {
            let suffix = if is_release { 'm' } else { 'M' };
            return Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes());
        }

        // Legacy X10 byte encoding: release replaces the button bits with 3
        // and coordinates saturate at 223 (255 - 32).
        let code = if is_release { (code & !0b11) | 3 } else { code };
        let encode_coord = |value: usize| -> u8 { (value + 1).min(223) as u8 + 32 };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (code as u8).saturating_add(32),
            encode_coord(col),
            encode_coord(row),
        ])
    }
}

impl TerminalGrid {
    /// Kitty keyboard stack for the screen currently in use.
    pub(super) fn kitty_kbd_mut(&mut self) -> &mut KittyKeyboardStack {
        if self.saved_screen.is_some() {
            &mut self.kitty_kbd_alt
        } else {
            &mut self.kitty_kbd_main
        }
    }

    /// Currently effective kitty keyboard flags (active screen's stack top).
    pub(super) fn kitty_keyboard_flags(&self) -> u8 {
        if self.saved_screen.is_some() {
            self.kitty_kbd_alt.current()
        } else {
            self.kitty_kbd_main.current()
        }
    }
}
