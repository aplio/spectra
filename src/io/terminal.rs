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
    if let Err(err) = execute!(
        stdout,
        terminal::EnterAlternateScreen,
        terminal::Clear(ClearType::All),
        EnableBracketedPaste,
        EnableMouseCapture,
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

#[cfg(test)]
mod tests {
    use super::{osc2_title_sequence, osc9_notification_sequence};

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
