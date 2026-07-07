use std::io::stdout;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    cursor::{self, SetCursorStyle},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, ClearType},
};

/// Whether kitty keyboard-enhancement flags were pushed to the host
/// terminal during [`setup`] and must be popped again on teardown (and in
/// the panic-hook restore path).
static KEYBOARD_ENHANCEMENT_PUSHED: AtomicBool = AtomicBool::new(false);

/// Enter raw mode, alternate screen, and install panic hook.
///
/// On failure the terminal is restored to cooked mode before the error is
/// returned, so callers can simply propagate it.
pub fn setup() -> std::io::Result<std::io::Stdout> {
    let mut stdout = stdout();
    terminal::enable_raw_mode()?;
    // Host mouse capture is NOT enabled here: it is driven dynamically by
    // the server (see `mouse_capture_sequence`), so the host terminal keeps
    // native mouse behaviour - most importantly link hover/click, which
    // terminals like ghostty disable entirely while an application has
    // mouse reporting active - whenever neither spectra ([mouse] enabled)
    // nor a guest program needs mouse events.
    if let Err(err) = execute!(
        stdout,
        terminal::EnterAlternateScreen,
        terminal::Clear(ClearType::All),
        EnableBracketedPaste,
        cursor::Show,
    ) {
        let _ = terminal::disable_raw_mode();
        return Err(err);
    }

    // Ask the host terminal for kitty-keyboard disambiguated key reports so
    // richer key information reaches the server for panes whose guests
    // enable the kitty keyboard protocol. Guarded: pushed only when the
    // host terminal advertises support (e.g. ghostty/kitty), best-effort
    // otherwise.
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true))
        && execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )
        .is_ok()
    {
        KEYBOARD_ENHANCEMENT_PUSHED.store(true, Ordering::SeqCst);
    }

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if KEYBOARD_ENHANCEMENT_PUSHED.load(Ordering::SeqCst) {
            let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            SetCursorStyle::DefaultUserShape,
            DisableBracketedPaste,
            DisableMouseCapture,
            terminal::LeaveAlternateScreen
        );
        default_hook(info);
    }));

    Ok(stdout)
}

/// Restore terminal to normal state.
pub fn teardown(mut stdout: std::io::Stdout) {
    if KEYBOARD_ENHANCEMENT_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        stdout,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste,
        DisableMouseCapture,
        terminal::LeaveAlternateScreen
    );
}

/// Build the ANSI sequence that enables or disables host-terminal mouse
/// capture. Sent by the server to attached clients when the need for mouse
/// events changes (spectra's [mouse] config, or a guest requesting mouse
/// reporting), so the host terminal is only captured while someone actually
/// consumes mouse input. Disabling when never enabled is harmless.
pub fn mouse_capture_sequence(enable: bool) -> String {
    use crossterm::Command;

    let mut ansi = String::new();
    // Writing into a String cannot fail.
    let _ = if enable {
        EnableMouseCapture.write_ansi(&mut ansi)
    } else {
        DisableMouseCapture.write_ansi(&mut ansi)
    };
    ansi
}

/// Build an OSC 2 escape sequence for setting the host window title.
pub fn osc2_title_sequence(title: &str) -> String {
    format!("\x1b]2;{title}\x07")
}

/// Build an OSC 9 desktop-notification escape sequence, stripping control
/// characters from the message so it cannot break out of the sequence.
pub fn osc9_notification_sequence(message: &str) -> String {
    let sanitized: String = message.chars().filter(|ch| !ch.is_control()).collect();
    format!("\x1b]9;{sanitized}\x07")
}

/// Build a ConEmu OSC 9;4 progress sequence for the host terminal.
/// `None` removes the indicator.
pub fn osc94_progress_sequence(
    progress: Option<crate::session::terminal_state::ProgressReport>,
) -> String {
    use crate::session::terminal_state::ProgressState;

    let Some(report) = progress else {
        return "\x1b]9;4;0\x07".to_string();
    };
    let state = match report.state {
        ProgressState::Normal => 1,
        ProgressState::Error => 2,
        ProgressState::Indeterminate => 3,
        ProgressState::Paused => 4,
    };
    match report.percent {
        Some(percent) => format!("\x1b]9;4;{state};{percent}\x07"),
        None => format!("\x1b]9;4;{state}\x07"),
    }
}

/// Build an OSC 12 escape sequence setting the host cursor color.
pub fn osc12_cursor_color_sequence((r, g, b): (u8, u8, u8)) -> String {
    format!("\x1b]12;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x07")
}

/// Build an OSC 112 escape sequence restoring the host's default cursor
/// color.
pub fn osc112_reset_cursor_color_sequence() -> String {
    "\x1b]112\x07".to_string()
}

#[cfg(test)]
mod tests {
    use super::{mouse_capture_sequence, osc2_title_sequence, osc9_notification_sequence};

    #[test]
    fn mouse_capture_sequence_toggles_xterm_mouse_modes() {
        let enable = mouse_capture_sequence(true);
        assert!(enable.contains("\x1b[?1000h"), "enable={enable:?}");
        assert!(enable.contains("\x1b[?1006h"), "enable={enable:?}");
        assert!(!enable.contains('l'), "enable={enable:?}");

        let disable = mouse_capture_sequence(false);
        assert!(disable.contains("\x1b[?1000l"), "disable={disable:?}");
        assert!(disable.contains("\x1b[?1006l"), "disable={disable:?}");
        assert!(!disable.contains('h'), "disable={disable:?}");
    }

    #[test]
    fn osc2_title_sequence_uses_bell_terminator() {
        assert_eq!(osc2_title_sequence("build"), "\x1b]2;build\x07");
    }

    #[test]
    fn osc9_notification_sequence_strips_control_characters() {
        assert_eq!(
            osc9_notification_sequence("spectra: claude blocked (pane 3)"),
            "\x1b]9;spectra: claude blocked (pane 3)\x07"
        );
        assert_eq!(
            osc9_notification_sequence("bad\x07\x1b]2;title\rmessage"),
            "\x1b]9;bad]2;titlemessage\x07"
        );
    }
}
